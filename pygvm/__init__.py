import posix_ipc
import mmap
import os
import struct
import subprocess
import ujson
import numpy as np

from typing import List
from vllm.sequence import (
    SequenceGroupMetadata,
)
from vllm.core.scheduler import SchedulerOutputs
from transformers import PreTrainedTokenizer

# macOS has 31 character name limit, so keep this short
# (Linux has 255)
DEFAULT_SHM_PREF = "/gvm0-"

DEFAULT_RT_PATH = "./gvm/gvmrt/target/debug/gvmrt"


def mkshm(name, size):
    shm_name = name + "-shm"
    # clean up just in case
    try:
        posix_ipc.unlink_shared_memory(shm_name)
    except:
        pass
    shm = posix_ipc.SharedMemory(shm_name, flags=posix_ipc.O_CREAT, size=size)
    map_file = mmap.mmap(shm.fd, size)
    os.close(shm.fd)

    return map_file


class MessageChannel:
    def __init__(self, name, size):
        write_sem_name = name + "-wr"
        read_sem_name = name + "-rd"

        # clean up just in case
        try:
            posix_ipc.unlink_semaphore(write_sem_name)
        except:
            pass
        try:
            posix_ipc.unlink_semaphore(read_sem_name)
        except:
            pass

        self.size = size
        self.map_file = mkshm(name, size)

        self.write_sem = posix_ipc.Semaphore(
            write_sem_name, flags=posix_ipc.O_CREAT, initial_value=1
        )

        self.read_sem = posix_ipc.Semaphore(
            read_sem_name, flags=posix_ipc.O_CREAT, initial_value=0
        )

    def send_bytes(self, msg_bytes):
        self.write_sem.acquire()
        self.map_file.seek(0)
        self.map_file.write(struct.pack("<I", len(msg_bytes)))
        self.map_file.write(msg_bytes)
        self.read_sem.release()

    def send_json(self, obj):
        self.send_bytes(ujson.dumps(obj).encode())

    def recv(self):
        self.read_sem.acquire()
        self.map_file.seek(0)
        msg_len_bytes = self.map_file.read(4)
        msg_len = struct.unpack("<I", msg_len_bytes)[0]
        msg = self.map_file.read(msg_len)
        self.write_sem.release()
        return msg

    def recv_json(self):
        return ujson.loads(self.recv())

    def close(self):
        self.map_file.close()
        # self.shm.unlink()
        # self.write_sem.unlink()
        # self.read_sem.unlink()


class GvmIface:
    instance = None

    def __init__(
        self,
        json_size=8,
        bin_size=16,
        pref=DEFAULT_SHM_PREF,
        rtpath=DEFAULT_RT_PATH,
    ) -> None:
        M = 1024 * 1024

        # self.vocab_size = 50257
        self.vocab_size = -1
        self.batch_size = -1

        self.logit_pending = False
        self.cmd_pending = False

        self.cmd_ch = MessageChannel(pref + "cmd", json_size * M)
        self.resp_ch = MessageChannel(pref + "resp", json_size * M)
        self.bin_shm = mkshm(pref + "bin", bin_size * M)

        self.proc = subprocess.Popen(
            [
                rtpath,
                "--json-size=" + str(json_size),
                "--bin-size=" + str(bin_size),
                "--name=" + pref,
                "--server",
            ],
        )

        self._cmd_and_resp("ping")

        GvmIface.instance = self

    def _send_cmd(self, data):
        assert not self.cmd_pending
        self.cmd_pending = True
        self.cmd_ch.send_json(data)

    def _cmd_and_resp(self, op: str, data={}):
        data["op"] = op
        self._send_cmd(data)
        self._expect_response("cmd:" + op)

    def _expect_response(self, ctx):
        assert self.cmd_pending
        self.cmd_pending = False
        resp = self.resp_ch.recv_json()
        if resp["type"] != "ok":
            raise ChildProcessError(
                f"Bad response ({ctx}): {ujson.dumps(resp)[0:1000]}"
            )
        return resp

    def set_tokenizer(self, tokenizer: PreTrainedTokenizer):
        self.vocab_size = tokenizer.vocab_size
        ids = [[id] for id in range(tokenizer.vocab_size)]
        tokens = tokenizer.batch_decode(ids)
        special = {
            "bos": tokenizer.bos_token_id,
            "eos": tokenizer.eos_token_id,
            "unk": tokenizer.unk_token_id,
            "sep": tokenizer.sep_token_id,
            "pad": tokenizer.pad_token_id,
            "cls": tokenizer.cls_token_id,
        }
        self._cmd_and_resp("tokens", {"special": special, "tokens": tokens})

    def step(
        self,
        freed_seq_ids: List[int],
        seq_group_metadata_list: List[SequenceGroupMetadata],
        scheduler_outputs: SchedulerOutputs,
    ):
        prompt_q = []
        gen_q = []
        for s in seq_group_metadata_list:
            ids = list(s.seq_data.keys())
            if s.is_prompt:
                assert len(ids) == 1
                id = ids[0]
                prompt_q.append(
                    {
                        "id": id,
                        "prompt": s.seq_data[id].prompt_token_ids,
                        "module_id": s.sampling_params.wasm_module_id,
                        "module_arg": s.sampling_params.wasm_arg,
                    }
                )
            else:
                for id in ids:
                    out = s.seq_data[id].output_token_ids
                    obj = {"id": id, "gen": out[-1]}
                    if len(out) == 1 and id != ids[0]:
                        obj["clone_id"] = ids[0]
                    gen_q.append(obj)
        cmd = {
            "op": "step",
            "freed": freed_seq_ids,
            "ops": prompt_q + gen_q,
        }
        self.batch_size = len(cmd["ops"])
        # self.scheduler.freed_seq_ids = []
        assert not self.logit_pending
        self.logit_pending = True
        self._send_cmd(cmd)

        # print(ujson.dumps(cmd))
        # for s in seq_group_metadata_list:
        #     if s.is_prompt: continue
        #     print(f'{"Prompt" if s.is_prompt else "Gen"} req={s.request_id}')
        #     for (id, data) in s.seq_data.items():
        #         # id is unique for run (so also unique for MT)
        #         print(f'    {id}: {data}')

    def flush_logit_bias(self):
        if self.logit_pending:
            print("Warning: unflushed Gvm logit bias")
            self.logit_pending = False
            self._expect_response("flush")

    def recv_logit_bias(self):
        assert self.logit_pending
        self.logit_pending = False
        self._expect_response("recv")
        n = self.batch_size
        arr = np.frombuffer(
            self.bin_shm, dtype=np.float32, offset=0, count=n * self.vocab_size
        ).reshape([n, self.vocab_size])
        return arr

    def stop(self):
        self._send_cmd({"op": "stop"})
        self.proc.wait()