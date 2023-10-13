#!/usr/bin/env python
import subprocess
import requests
import ujson
import sys
import os

base_url = "http://127.0.0.1:8080/v1/"
prog = "aici_ast_runner"

ast = {
    "steps": [
        {"Fixed": {"text": "I WAS about "}},
        {"Gen": {"max_tokens": 5, "rx": "\\d\\d"}},
        {"Fixed": {"text": " years and "}},
        {"Gen": {"max_tokens": 5, "rx": "\\d+"}},
    ]
}


def upload_wasm():
    r = subprocess.run(["sh", "wasm.sh"], cwd=prog)
    if r.returncode != 0:
        sys.exit(1)
    file_path = prog + "/target/opt.wasm"
    print("upload module... ", end="")
    with open(file_path, "rb") as f:
        resp = requests.post(base_url + "aici_modules", data=f)
        if resp.status_code == 200:
            d = resp.json()
            dd = d["data"]
            mod_id = dd["module_id"]
            print(
                f"{dd['wasm_size']//1024}kB -> {dd['compiled_size']//1024}kB id:{mod_id[0:8]}"
            )
            return mod_id
        else:
            raise RuntimeError(
                f"bad response to model upload: {resp.status_code} {resp.reason}: {resp.text}"
            )


def ask_completion(prompt, aici_module, aici_arg, temperature=0, max_tokens=200):
    json = {
        "model": "",
        "prompt": prompt,
        "max_tokens": max_tokens,
        "temperature": temperature,
        "stream": True,
        "aici_module": aici_module,
        "aici_arg": aici_arg,
    }
    resp = requests.post(base_url + "completions", json=json, stream=True)
    if resp.status_code != 200:
        raise RuntimeError(
            f"bad response to completions: {resp.status_code} {resp.reason}: {resp.text}"
        )
    full_resp = []
    for line in resp.iter_lines():
        if line:
            decoded_line: str = line.decode("utf-8")
            if decoded_line.startswith("data: {"):
                d = ujson.decode(decoded_line[6:])
                full_resp.append(d)
                i0 = [ch for ch in d["choices"] if ch["index"] == 0]
                if len(i0) > 0:
                    print(i0[0]["text"], end="")
            elif decoded_line == "data: [DONE]":
                print(" [DONE]")
            else:
                print(decoded_line)

    os.makedirs("tmp", exist_ok=True)
    path = "tmp/response.json"
    with open(path, "w") as f:
        ujson.dump({"request": json, "response": full_resp}, f, indent=1)
    print(f"response saved to {path}")


def main():
    mod = upload_wasm()
    ask_completion(prompt="42\n", aici_module=mod, aici_arg=ast)


main()
