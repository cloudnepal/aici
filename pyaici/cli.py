import subprocess
import ujson
import sys
import os
import argparse

from . import rest


def cli_error(msg: str):
    print("Error: " + msg)
    sys.exit(1)


def build_rust(folder: str):
    bin_file = ""
    spl = folder.split("::")
    if len(spl) > 1:
        folder = spl[0]
        bin_file = spl[1]
    r = subprocess.run(
        [
            "cargo",
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version=1",
        ],
        cwd=folder,
        stdout=-1,
        check=True,
    )
    info = ujson.decode(r.stdout)
    if len(info["workspace_default_members"]) != 1:
        cli_error("please run from project, not workspace, folder")
    pkg_id = info["workspace_default_members"][0]
    pkg = [pkg for pkg in info["packages"] if pkg["id"] == pkg_id][0]

    bins = [trg for trg in pkg["targets"] if trg["kind"] == ["bin"]]
    if len(bins) == 0:
        cli_error("no bin targets found")
    bins_str = ", ".join([folder + "::" + trg["name"] for trg in bins])
    if bin_file:
        if len([trg for trg in bins if trg["name"] == bin_file]) == 0:
            cli_error(f"{bin_file} not found; try one of {bins_str}")
    else:
        if len(bins) > 1:
            cli_error("more than one bin target found; use one of: " + bins_str)
        bin_file = bins[0]["name"]
    print(f'will build {bin_file} from {pkg["manifest_path"]}')

    triple = "wasm32-wasi"
    trg_path = (
        info["target_directory"] + "/" + triple + "/release/" + bin_file + ".wasm"
    )
    # remove file first, so we're sure it's rebuilt
    try:
        os.unlink(trg_path)
    except:
        pass
    r = subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "--target",
            triple,
        ],
        cwd=folder,
    )
    if r.returncode != 0:
        sys.exit(1)
    bb = open(trg_path, "rb").read()
    M = 1024 * 1024
    print(f"built: {trg_path}, {len(bb)/M:.3} MiB")
    return rest.upload_module(trg_path)


def ask_completion(cmd_args, *args, **kwargs):
    if cmd_args is not None:
        for k in ["max_tokens", "prompt", "ignore_eos"]:
            v = getattr(cmd_args, k)
            if v is not None:
                kwargs[k] = v
    res = rest.completion(*args, **kwargs)
    print("\n[Prompt] " + res["request"]["prompt"] + "\n")
    for text in res["text"]:
        print("[Response] " + text + "\n")
    os.makedirs("tmp", exist_ok=True)
    path = "tmp/response.json"
    with open(path, "w") as f:
        ujson.dump(res, f, indent=1)
    print(f"response saved to {path}")
    print("Usage:", res["usage"])
    print("Storage:", res["storage"])


def infer_args(cmd: argparse.ArgumentParser):
    cmd.add_argument("--prompt", "-p", default="", type=str, help="specify prompt")
    cmd.add_argument(
        "--max-tokens", "-t", type=int, help="maximum number of tokens to generate"
    )


def upload_main():
    import __main__

    main_script_file = __main__.__file__
    print("Pretending you wanted to say:")
    print(f"pyaici run {main_script_file}\n")

    rest.require_explicit_base_url()

    aici_arg = open(main_script_file).read()
    rest.log_level = 3
    ask_completion(
        None,
        prompt="",
        aici_module="pyvm-latest",
        aici_arg=aici_arg,
        ignore_eos=True,
        max_tokens=2000,
    )


def main_inner():
    parser = argparse.ArgumentParser(
        description="Upload an AICI VM and completion request to rllm or vllm",
        prog="pyaici",
    )

    parser.add_argument(
        "--log-level",
        "-l",
        type=int,
        help="log level (higher is more); default 3 (except in 'infer', where it's 1)",
    )

    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    run_cmd = subparsers.add_parser(
        "run",
        help="run model inference controlled by a VM",
        description="Run model inference controlled by a VM.",
        epilog="""
        If FILE ends with .py, --vm defaults to 'pyvm'. For .json file it defaults to 'declvm'.
        """,
    )
    run_cmd.add_argument(
        "aici_arg", metavar="FILE", nargs="?", help="file to pass to the VM"
    )
    infer_args(run_cmd)
    run_cmd.add_argument(
        "--vm",
        "-v",
        metavar="MODULE_ID",
        type=str,
        help="tag name or hex module id",
    )
    run_cmd.add_argument(
        "--upload",
        "-u",
        metavar="WASM_FILE",
        type=str,
        help="path to .wasm file to upload; shorthand for 'pyaici upload WASM_FILE'",
    )
    run_cmd.add_argument(
        "--build",
        "-b",
        metavar="FOLDER",
        type=str,
        help="path to rust project to build and upload; shorthand for 'pyaici build FOLDER'",
    )

    infer_cmd = subparsers.add_parser(
        "infer",
        help="run model inference without any VM",
        description="Run model inference without any VM.",
    )
    infer_args(infer_cmd)
    infer_cmd.add_argument(
        "--ignore-eos", action="store_true", help="ignore EOS tokens generated by model"
    )

    tags_cmd = subparsers.add_parser(
        "tags",
        help="list module tags available on the server",
        description="List module tags available on the server.",
    )

    upload_cmd = subparsers.add_parser(
        "upload",
        help="upload a VM to the server",
        description="Upload a VM to the server.",
    )
    upload_cmd.add_argument(
        "upload", metavar="WASM_FILE", help="path to .wasm file to upload"
    )

    build_cmd = subparsers.add_parser(
        "build",
        help="build and upload a VM to the server",
        description="Build and upload a VM to the server.",
    )
    build_cmd.add_argument(
        "build", metavar="FOLDER", help="path to rust project (folder with Cargo.toml)"
    )

    for cmd in [upload_cmd, build_cmd]:
        cmd.add_argument(
            "--tag",
            "-T",
            type=str,
            default=[],
            action="append",
            help="tag the VM after uploading; can be used multiple times to set multiple tags",
        )

    args = parser.parse_args()

    if args.log_level:
        rest.log_level = args.log_level
    else:
        rest.log_level = 3

    if args.subcommand == "tags":
        for tag in rest.list_tags():
            print(rest.pp_tag(tag))
        sys.exit(0)

    if args.subcommand == "infer":
        if args.prompt == "":
            cli_error("--prompt empty")
        # for plain prompting, use log-level 1 by default
        if args.log_level is None:
            rest.log_level = 1
        ask_completion(
            args,
            aici_module=None,
            aici_arg=None,
            max_tokens=100,
        )
        sys.exit(0)

    aici_module = ""

    for k in ["build", "upload", "vm", "tag", "ignore_eos"]:
        if k not in args:
            setattr(args, k, None)

    if args.build:
        assert not aici_module
        aici_module = build_rust(args.build)

    if args.upload:
        assert not aici_module
        aici_module = rest.upload_module(args.upload)

    if args.vm:
        assert not aici_module
        aici_module = args.vm

    if args.tag:
        if len(aici_module) != 64:
            cli_error("no VM to tag")
        rest.tag_module(aici_module, args.tag)

    if args.subcommand == "run":
        aici_arg = ""
        fn: str = args.aici_arg
        if fn is not None:
            aici_arg = open(fn).read()
            if not aici_module:
                if fn.endswith(".py"):
                    aici_module = "pyvm-latest"
                elif fn.endswith(".json"):
                    aici_module = "declvm-latest"
                else:
                    cli_error("Can't determine VM type from file name: " + fn)
                print(f"Running with tagged vm: {aici_module}")
        if not aici_module:
            cli_error("no VM specified to run")

        ask_completion(
            args,
            aici_module=aici_module,
            aici_arg=aici_arg,
            ignore_eos=True,
            max_tokens=2000,
        )


def main():
    try:
        main_inner()
    except RuntimeError as e:
        cli_error(str(e))


if __name__ == "__main__":
    main()
