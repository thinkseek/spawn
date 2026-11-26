#!/usr/bin/env python3
"""JSON-RPC fault-injection proxy in front of `spawn-hc` (herdr-core) for scenarios."""

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time


def load_lines(path):
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return [line.strip() for line in fh if line.strip()]
    except OSError:
        return []


def load_trimmed(path):
    try:
        with open(path, "r", encoding="utf-8") as fh:
            return fh.read().strip()
    except OSError:
        return ""


def terminal_matches(label, target):
    if not label or not target:
        return False
    return label == target or label.removeprefix("spawn-") == target.removeprefix("spawn-")


def norm(text):
    return (text or "").lower().replace("_", "-")


class FaultPolicy:
    def __init__(self, args):
        self.log_path = args.log
        self.read_delay_ms = args.read_delay_ms
        self.refuse_stage_terminals = args.refuse_stage_terminals
        self.refuse_stage_file = args.refuse_stage_file
        self.refuse_stage_agents = []
        for spec in args.refuse_stage_agent or []:
            flag_file, substr = spec.split(":", 1)
            self.refuse_stage_agents.append((flag_file, substr))
        self.refuse_kill_file = args.refuse_kill_file
        self.log_lock = threading.Lock()

    def record(self, method):
        if not self.log_path:
            return
        name = "capture-pane" if method == "terminal.read" else method
        ts_ms = int(time.time() * 1000)
        with self.log_lock:
            with open(self.log_path, "a", encoding="utf-8") as fh:
                fh.write(f"{ts_ms} {name}\n")

    def maybe_delay(self, method):
        if method == "terminal.read" and self.read_delay_ms > 0:
            time.sleep(self.read_delay_ms / 1000.0)

    def refuses_kill(self):
        return bool(self.refuse_kill_file and os.path.exists(self.refuse_kill_file))

    def should_pend_stage(self, info):
        labels = info.get("labels") if isinstance(info, dict) else None
        if not isinstance(labels, dict):
            return False
        term = labels.get("spawn_terminal", "")
        ws = labels.get("spawn_workspace", "")

        if self.refuse_stage_terminals:
            for item in load_lines(self.refuse_stage_terminals):
                if terminal_matches(term, item):
                    return True

        if self.refuse_stage_file:
            held = load_trimmed(self.refuse_stage_file)
            if held and terminal_matches(term, held):
                return True

        norm_ws = norm(ws)
        norm_term = norm(term)
        for flag_file, substr in self.refuse_stage_agents:
            if os.path.exists(flag_file):
                needle = norm(substr)
                if needle in norm_ws or needle in norm_term:
                    return True

        return False


def relay_upstream_to_client(up_sock, cli_sock, pending, pending_lock, policy):
    buf = b""
    try:
        while True:
            chunk = up_sock.recv(65536)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                raw_line, buf = buf.split(b"\n", 1)
                out_line = raw_line
                try:
                    msg = json.loads(raw_line.decode("utf-8"))
                except Exception:
                    msg = None
                if isinstance(msg, dict) and "id" in msg and "result" in msg:
                    req_id = msg.get("id")
                    with pending_lock:
                        method = pending.pop(req_id, None)
                    if method == "terminal.get" and isinstance(msg.get("result"), dict):
                        if policy.should_pend_stage(msg["result"]):
                            msg["result"]["reason"] = "spawn_pending"
                            out_line = json.dumps(msg, separators=(",", ":")).encode("utf-8")
                cli_sock.sendall(out_line + b"\n")
    except OSError:
        pass
    finally:
        try:
            cli_sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle_client(cli_sock, upstream_path, policy):
    try:
        up_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        up_sock.connect(upstream_path)
    except OSError:
        cli_sock.close()
        return

    pending = {}
    pending_lock = threading.Lock()
    t = threading.Thread(
        target=relay_upstream_to_client,
        args=(up_sock, cli_sock, pending, pending_lock, policy),
        daemon=True,
    )
    t.start()

    buf = b""
    try:
        while True:
            chunk = cli_sock.recv(65536)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                raw_line, buf = buf.split(b"\n", 1)
                try:
                    msg = json.loads(raw_line.decode("utf-8"))
                except Exception:
                    msg = None
                if isinstance(msg, dict) and isinstance(msg.get("method"), str):
                    method = msg["method"]
                    req_id = msg.get("id")
                    policy.record(method)
                    policy.maybe_delay(method)
                    if method == "terminal.kill" and policy.refuses_kill():
                        if req_id is not None:
                            err = {
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": {
                                    "code": -32000,
                                    "message": "kill-session refused by the scenario",
                                    "data": {"kind": "refused"},
                                },
                            }
                            cli_sock.sendall((json.dumps(err) + "\n").encode("utf-8"))
                        continue
                    if req_id is not None:
                        with pending_lock:
                            pending[req_id] = method
                up_sock.sendall(raw_line + b"\n")
    except OSError:
        pass
    finally:
        try:
            up_sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        t.join(timeout=2.0)
        up_sock.close()
        cli_sock.close()


def socket_alive(path):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(path)
        s.close()
        return True
    except OSError:
        return False


def start_upstream(program, session, sock_path):
    if socket_alive(sock_path):
        return
    os.makedirs(os.path.dirname(sock_path), mode=0o700, exist_ok=True)
    if os.path.exists(sock_path):
        try:
            os.unlink(sock_path)
        except OSError:
            pass
    env = os.environ.copy()
    env["HC_SESSION"] = session
    env["HC_SOCKET"] = sock_path
    res = subprocess.run([program, "server", "start"], env=env, capture_output=True, text=True)
    if res.returncode != 0 and not socket_alive(sock_path):
        raise RuntimeError(f"upstream server start failed ({res.returncode}): {res.stderr.strip()}")
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        if socket_alive(sock_path):
            return
        time.sleep(0.02)
    raise RuntimeError(f"upstream socket {sock_path} did not accept connections")


def run_server(args, ready_write_fd=None):
    if args.upstream_program:
        start_upstream(args.upstream_program, args.upstream_session, args.upstream_socket)

    listen_dir = os.path.dirname(args.listen)
    os.makedirs(listen_dir, mode=0o700, exist_ok=True)
    if os.path.exists(args.listen):
        if socket_alive(args.listen):
            if ready_write_fd is not None:
                os.write(ready_write_fd, b"ready\n")
                os.close(ready_write_fd)
            return
        os.unlink(args.listen)

    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(args.listen)
    os.chmod(args.listen, 0o600)
    srv.listen(128)

    if args.pidfile:
        with open(args.pidfile, "w", encoding="utf-8") as fh:
            fh.write(f"{os.getpid()}\n")

    if ready_write_fd is not None:
        os.write(ready_write_fd, b"ready\n")
        os.close(ready_write_fd)

    policy = FaultPolicy(args)
    stopping = False

    def handle_sig(_signum, _frame):
        nonlocal stopping
        stopping = True
        try:
            srv.close()
        except OSError:
            pass

    signal.signal(signal.SIGTERM, handle_sig)
    signal.signal(signal.SIGINT, handle_sig)

    while not stopping:
        try:
            cli, _ = srv.accept()
        except OSError:
            break
        threading.Thread(
            target=handle_client,
            args=(cli, args.upstream_socket, policy),
            daemon=True,
        ).start()

    try:
        if os.path.exists(args.listen):
            os.unlink(args.listen)
    except OSError:
        pass


def parse_args():
    parser = argparse.ArgumentParser(description="Fault-injection JSON-RPC proxy for spawn-hc")
    parser.add_argument("--server-start", action="store_true")
    parser.add_argument("--upstream-program")
    parser.add_argument("--upstream-session")
    parser.add_argument("--upstream-socket", required=True)
    parser.add_argument("--listen", required=True)
    parser.add_argument("--pidfile")
    parser.add_argument("--args-file")
    parser.add_argument("--log")
    parser.add_argument("--read-delay-ms", type=int, default=0)
    parser.add_argument("--refuse-stage-terminals")
    parser.add_argument("--refuse-stage-file")
    parser.add_argument("--refuse-stage-agent", action="append", default=[])
    parser.add_argument("--refuse-kill-file")
    args, unknown = parser.parse_known_args()
    if args.args_file and os.path.exists(args.args_file):
        with open(args.args_file, "r", encoding="utf-8") as fh:
            extra = json.load(fh)
        args = parser.parse_args(extra + sys.argv[1:])
    elif unknown:
        parser.error(f"unrecognized arguments: {unknown}")
    return args


def main():
    args = parse_args()
    if socket_alive(args.listen):
        return 0
    if args.server_start:
        if args.upstream_program:
            start_upstream(args.upstream_program, args.upstream_session, args.upstream_socket)
        rfd, wfd = os.pipe()
        pid = os.fork()
        if pid > 0:
            os.close(wfd)
            data = os.read(rfd, 32)
            os.close(rfd)
            return 0 if data.startswith(b"ready") else 1
        os.close(rfd)
        os.setsid()
        devnull = os.open(os.devnull, os.O_RDWR)
        os.dup2(devnull, 0)
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
        if devnull > 2:
            os.close(devnull)
        run_server(args, ready_write_fd=wfd)
        return 0
    run_server(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
