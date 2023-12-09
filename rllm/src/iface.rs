use aici_abi::{
    bytes::{limit_bytes, limit_str},
    toktree::TokTrie,
};
use aicirt::{
    api::{
        AiciPreProcessReq, AiciPreProcessResp, InstantiateReq, MkModuleReq, MkModuleResp,
        TokensResp,
    },
    msgchannel::MessageChannel,
    shm::Shm,
};
use anyhow::Result;
use futures::future::select_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::{
    process::{Child, Command},
    time::Duration,
};
use tokio::{signal::unix::SignalKind, sync::oneshot};

pub struct CmdChannel {
    cmd_pending: bool,
    cmd_ch: MessageChannel,
    resp_ch: MessageChannel,
    busy_wait_duration: Duration,
}

const M: usize = 1 << 20;

impl CmdChannel {
    pub fn new(
        json_size: usize,
        pref: &str,
        suff: &str,
        busy_wait_duration: Duration,
    ) -> Result<Self> {
        Ok(Self {
            cmd_pending: false,
            cmd_ch: MessageChannel::new(&format!("{}cmd{}", pref, suff), json_size * M)?,
            resp_ch: MessageChannel::new(&format!("{}resp{}", pref, suff), json_size * M)?,
            busy_wait_duration,
        })
    }

    pub fn send_bytes(&mut self, data: &[u8]) -> Result<()> {
        assert!(!self.cmd_pending);
        self.cmd_pending = true;
        self.cmd_ch.send(data)?;
        Ok(())
    }

    pub fn exec<T: Serialize, R>(&mut self, op: &str, data: T) -> Result<R>
    where
        R: for<'d> Deserialize<'d>,
    {
        self.send(op, data)?;
        self.expect(&format!("cmd:{}", op))
    }

    pub fn send<T: Serialize>(&mut self, op: &str, data: T) -> Result<()> {
        let mut value = serde_json::to_value(data)?;
        value["op"] = json!(op);
        let bytes = serde_json::to_vec(&value)?;
        self.send_bytes(&bytes)
    }

    pub fn expect<R>(&mut self, ctx: &str) -> Result<R>
    where
        R: for<'d> Deserialize<'d>,
    {
        assert!(self.cmd_pending);
        let bytes = self.resp_ch.recv(&self.busy_wait_duration)?;
        self.cmd_pending = false;
        let mut resp: Value = serde_json::from_slice(&bytes)?;
        if resp["type"] != "ok" {
            return Err(anyhow::anyhow!(
                "Bad response ({ctx}): {}",
                limit_bytes(&bytes, 500)
            ));
        }
        let data = resp
            .as_object_mut()
            .unwrap()
            .remove("data")
            .ok_or(anyhow::anyhow!(
                "Bad response ({ctx}) - no 'data': {}",
                limit_bytes(&bytes, 500)
            ))?;
        let resp = serde_json::from_value(data)?;
        Ok(resp)
    }
}

pub struct AiciRtIface {
    cmd: CmdChannel,
    pub bin_shm: Shm,
    pub side_cmd: AsyncCmdChannel,
    #[allow(dead_code)]
    child: Child,
}

pub struct Args {
    pub aicirt: String,
    pub tokenizer: String,
    pub json_size: usize,
    pub bin_size: usize,
    pub shm_prefix: String,
    pub busy_wait_time: u64,
}

impl AiciRtIface {
    pub fn start_aicirt(args: &Args, tok_trie: &TokTrie) -> Result<Self> {
        let busy_wait_time = Duration::from_millis(args.busy_wait_time);
        let shm_name = MessageChannel::shm_name(&args.shm_prefix) + "-bin";
        let cmd = CmdChannel::new(args.json_size, &args.shm_prefix, "", busy_wait_time)?;
        let side_cmd = AsyncCmdChannel::new(args.json_size, &args.shm_prefix, "-side")?;
        let bin_shm = Shm::new(&shm_name, args.bin_size * M)?;

        let child = Command::new(&args.aicirt)
            .arg("--tokenizer")
            .arg(&args.tokenizer)
            .arg("--json-size")
            .arg(&args.json_size.to_string())
            .arg("--bin-size")
            .arg(&args.bin_size.to_string())
            .arg("--name")
            .arg(&args.shm_prefix)
            .arg("--server")
            .spawn()?;

        let pid = child.id() as libc::c_int;
        let default_panic_hook = std::panic::take_hook();

        std::panic::set_hook(Box::new(move |panic_info| {
            eprintln!("killing {pid}");
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            default_panic_hook(panic_info);
            std::process::exit(100);
        }));

        let _killer = tokio::spawn(async move {
            let sigs = vec![
                SignalKind::interrupt(),
                SignalKind::quit(),
                SignalKind::terminate(),
            ];

            let mut sigs = sigs
                .iter()
                .map(|s| tokio::signal::unix::signal(*s).unwrap())
                .collect::<Vec<_>>();

            loop {
                let futures: Vec<_> = sigs.iter_mut().map(|s| s.recv()).collect();
                let pinned_futures: Vec<_> = futures.into_iter().map(|f| Box::pin(f)).collect();
                select_all(pinned_futures).await;
                log::info!("Killing child process");
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                }
            }
        });

        let mut r = Self {
            cmd,
            side_cmd,
            bin_shm,
            child,
        };

        let _: Value = r.cmd.exec("ping", json!({}))?;
        let tokens: TokensResp = r.cmd.exec("tokens", json!({}))?;

        // well, this is somewhat unlikely as we're passing the same toknizer name down...
        if tokens.vocab_size != tok_trie.info().vocab_size {
            return Err(anyhow::anyhow!(
                "Vocab size mismatch: {:?} != {:?}",
                tokens,
                tok_trie.info()
            ));
        }

        Ok(r)
    }

    pub fn aici_pre(&mut self, req: AiciPreProcessReq) -> Result<AiciPreProcessResp> {
        self.cmd.exec("pre_process", req)
    }
}

#[derive(Clone)]
pub struct AsyncCmdChannel {
    pending_reqs: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    cmd_ch: Arc<Mutex<MessageChannel>>,
}

impl AsyncCmdChannel {
    pub fn new(json_size: usize, pref: &str, suff: &str) -> Result<Self> {
        let cmd = CmdChannel::new(json_size, pref, suff, Duration::ZERO)?;
        let pending_reqs = Arc::new(Mutex::new(HashMap::<String, oneshot::Sender<Value>>::new()));
        {
            let resp_ch = cmd.resp_ch;
            let pending_reqs = pending_reqs.clone();
            thread::spawn(move || loop {
                let resp = resp_ch.recv(&Duration::ZERO).unwrap();
                let resp: Value = serde_json::from_slice(&resp).unwrap();
                let rid = resp["$rid"].as_str().unwrap().to_string();
                let tx = pending_reqs.lock().unwrap().remove(&rid).unwrap();
                tx.send(resp).unwrap();
            });
        }

        Ok(Self {
            pending_reqs,
            cmd_ch: Arc::new(Mutex::new(cmd.cmd_ch)),
        })
    }

    pub async fn mk_module(&self, req: MkModuleReq) -> Result<MkModuleResp> {
        self.exec("mk_module", req).await
    }

    pub async fn instantiate(&self, req: InstantiateReq) -> Result<()> {
        self.exec("instantiate", req).await
    }

    pub async fn exec<T: Serialize, R>(&self, op: &str, data: T) -> Result<R>
    where
        R: for<'d> Deserialize<'d>,
    {
        let rid = uuid::Uuid::new_v4().to_string();
        let mut data = serde_json::to_value(data)?;
        data["op"] = Value::String(op.to_string());
        data["$rid"] = Value::String(rid.clone());

        let (tx, rx) = oneshot::channel();
        self.pending_reqs.lock().unwrap().insert(rid.clone(), tx);

        self.cmd_ch
            .lock()
            .unwrap()
            .send(&serde_json::to_vec(&data)?)?;

        let mut resp = rx.await?;

        match resp["type"].as_str() {
            Some("ok") => {
                let data = resp
                    .as_object_mut()
                    .unwrap()
                    .remove("data")
                    .ok_or(anyhow::anyhow!(
                        "Bad response  ({op}) - no 'data': {}",
                        limit_bytes(&serde_json::to_vec(&resp)?, 500)
                    ))?;
                let resp = serde_json::from_value(data)?;
                Ok(resp)
            }
            _ => {
                let info = match resp["error"].as_str() {
                    Some(text) => text.to_string(),
                    _ => serde_json::to_string(&resp)?,
                };
                Err(anyhow::anyhow!(
                    "Bad response  ({op}): {}",
                    limit_str(&info, 2000)
                ))
            }
        }
    }
}
