#!/usr/bin/env python3
"""Exercise the real Ferry CLI against a deterministic, local FTP peer.

No Docker or third-party modules. Run with:
  python3 tests/download_snapshot_regression.py --binary target/debug/ferry
The peer injects file changes at protocol boundaries and counts requests.
"""
import argparse
import json
import shutil
from pathlib import Path
import socket
import subprocess
import tempfile
import threading

OLD_TIME = "20261005100000"
NEW_TIME = "20261005100001"


class Peer:
    def __init__(self, mode="stable", metadata=True):
        self.mode, self.metadata = mode, metadata
        self.body, self.mtime = b"old!", OLD_TIME
        self.events, self.failure = [], None
        self.root = None
        self.changed = False
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while not self.stop.is_set():
                try:
                    conn, _ = self.listener.accept()
                except socket.timeout:
                    continue
                with conn:
                    conn.settimeout(5)
                    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    self.session(conn)
        except Exception as error:
            self.failure = error

    def session(self, conn):
        passive = None
        def reply(text):
            conn.sendall((text + "\r\n").encode())
        reply("220 deterministic test FTP")
        try:
            with conn.makefile("rb") as reader:
                for raw in reader:
                    cmd, _, arg = raw.decode().rstrip("\r\n").partition(" ")
                    self.events.append(cmd)
                    path = "/" + arg.strip("/")
                    if cmd == "USER":
                        reply("331 password required")
                    elif cmd == "PASS":
                        reply("230 logged in")
                    elif cmd in ("TYPE", "NOOP"):
                        reply("200 OK")
                    elif cmd == "PWD":
                        reply('257 "/"')
                    elif cmd == "SIZE":
                        reply(f"213 {len(self.body)}" if path == "/file.c" else "550 not a file")
                    elif cmd == "MDTM":
                        if not self.metadata:
                            reply("502 MDTM unsupported")
                            continue
                        observed = self.mtime
                        reply("213 " + observed)
                        # The validated old bytes must keep their old timestamp.
                        # A later MDTM must not relabel them as the new version.
                        if self.mode == "after_validation" and not self.changed and "RETR" in self.events:
                            self.body, self.mtime, self.changed = b"new!", NEW_TIME, True
                    elif cmd == "PASV":
                        if passive:
                            passive.close()
                        passive = socket.socket()
                        passive.bind(("127.0.0.1", 0))
                        passive.listen()
                        passive.settimeout(5)
                        port = passive.getsockname()[1]
                        reply(f"227 Entering Passive Mode (127,0,0,1,{port // 256},{port % 256})")
                    elif cmd in ("LIST", "RETR"):
                        if cmd == "RETR" and path != "/file.c":
                            reply("550 missing")
                            continue
                        reply("150 opening data connection")
                        data, _ = passive.accept()
                        with data:
                            if cmd == "LIST":
                                payload = f"-rw-r--r-- 1 test test {len(self.body)} Oct 05 10:00 file.c\r\n".encode()
                            else:
                                payload = self.body
                            data.sendall(payload)
                        passive.close()
                        passive = None
                        if cmd == "RETR" and (self.mode == "during_transfer" or
                                              self.mode == "during_init_transfer" and self.events.count("RETR") == 2):
                            self.body, self.mtime = b"new!", NEW_TIME
                        reply("226 transfer complete")
                    elif cmd == "QUIT":
                        reply("221 bye")
                        break
                    else:
                        raise AssertionError(f"Unexpected FTP operation: {cmd} {arg}")
        finally:
            if passive:
                passive.close()

    def close(self):
        self.stop.set()
        self.thread.join(timeout=6)
        self.listener.close()
        assert not self.thread.is_alive(), "FTP peer failed to stop"
        assert self.failure is None, self.failure
        if self.root is not None:
            shutil.rmtree(self.root)


def config(root, peer):
    (root / ".ferry.toml").write_text(
        f'[connection]\nhost="127.0.0.1"\nport={peer.port}\nuser="test"\npassword="test"\n'
        f'[paths]\nlocal_root={json.dumps(str(root))}\nremote_root="/"\n'
        '[sync]\nignore=[".ferry/", ".ferry.toml"]\n'
    )


def invoke(binary, root, *args, success=True):
    result = subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, timeout=15)
    assert (result.returncode == 0) == success, (args, result.returncode, result.stdout, result.stderr)
    return result


def state(root):
    return json.loads((root / ".ferry/state.json").read_text())


def case(binary, mode, metadata=True):
    peer = Peer(mode, metadata)
    root = Path(tempfile.mkdtemp(prefix="ferry-snapshot-test-"))
    peer.root = root
    config(root, peer)
    return peer, root


def run(binary):
    peer, root = case(binary, "stable")
    try:
        invoke(binary, root, "pull", "file.c")
        assert (root / "file.c").read_bytes() == b"old!"
        assert peer.events.count("MDTM") == 2, peer.events
        assert peer.events.count("RETR") == 1
        before = len(peer.events)
        invoke(binary, root, "pull", "file.c")
        assert peer.events[before:].count("MDTM") == 1
        assert "RETR" not in peer.events[before:]
        original = (root / ".ferry/state.json").read_bytes()
        (root / "file.c").write_bytes(b"local edits")
        invoke(binary, root, "pull", "file.c", success=False)
        assert (root / "file.c").read_bytes() == b"local edits"
        invoke(binary, root, "--dry-run", "pull", "--force", "file.c")
        assert (root / "file.c").read_bytes() == b"local edits"
        assert (root / ".ferry/state.json").read_bytes() == original
        invoke(binary, root, "pull", "--force", "file.c")
        assert (root / "file.c").read_bytes() == b"old!"
        print("PASS: 2 MDTM per download; unchanged cache; conflicts; force; dry-run")
    finally:
        peer.close()

    peer, root = case(binary, "after_validation")
    try:
        invoke(binary, root, "pull", "file.c")
        assert state(root)["files"]["file.c"]["remote_mtime"] == "2026-10-05T10:00:00Z"
        invoke(binary, root, "pull", "file.c")
        assert (root / "file.c").read_bytes() == b"new!", "new remote version was incorrectly cached as old bytes"
        print("PASS: remote edit after validation is discovered on the next pull")
    finally:
        peer.close()

    peer, root = case(binary, "during_transfer")
    try:
        result = invoke(binary, root, "pull", "file.c", success=False)
        assert "remote changed while downloading" in result.stderr, result.stderr
        assert not (root / "file.c").exists()
        assert not (root / ".ferry/state.json").exists()
        print("PASS: remote edit during transfer is rejected without installing/state")
    finally:
        peer.close()

    peer, root = case(binary, "stable", metadata=False)
    try:
        invoke(binary, root, "pull", "file.c")
        assert (root / "file.c").read_bytes() == b"old!"
        assert state(root)["server_supports_mdtm"] is False
        before = len(peer.events)
        invoke(binary, root, "pull", "file.c")
        assert "MDTM" not in peer.events[before:]
        assert peer.events[before:].count("RETR") == 1
        print("PASS: unsupported MDTM preserves the uncached download fallback")
    finally:
        peer.close()

    peer, root = case(binary, "stable")
    try:
        invoke(binary, root, "--dry-run", "pull", "file.c")
        assert not (root / "file.c").exists()
        assert not (root / ".ferry/state.json").exists()
        print("PASS: fresh dry-run creates neither local payload nor state")
    finally:
        peer.close()


    for mode in ("stable", "after_validation", "during_transfer"):
        peer, root = case(binary, mode)
        try:
            invoke(binary, root, "sync", success=mode != "during_transfer")
            if mode == "during_transfer":
                assert not (root / "file.c").exists()
                assert not (root / ".ferry/state.json").exists()
            else:
                assert peer.events.count("MDTM") == 2, peer.events
                assert (root / "file.c").read_bytes() == b"old!"
                assert state(root)["files"]["file.c"]["remote_mtime"] == "2026-10-05T10:00:00Z"
                if mode == "after_validation":
                    invoke(binary, root, "sync")
                    assert (root / "file.c").read_bytes() == b"new!"
            print("PASS: project sync / " + mode)
        finally:
            peer.close()

    for mode in ("stable", "during_transfer"):
        peer, root = case(binary, mode)
        try:
            (root / ".ferry.toml").unlink()
            if mode == "during_transfer":
                peer.mode = "during_init_transfer"
            (root / "file.c").write_bytes(b"local edits")
            answers = f"127.0.0.1\n{peer.port}\ntest\ntest\n/\n.\nP\n"
            result = subprocess.run([binary, "init"], cwd=root, input=answers,
                                    capture_output=True, text=True, timeout=15)
            assert (result.returncode == 0) == (mode == "stable"), (result.stdout, result.stderr)
            if mode == "stable":
                assert (root / "file.c").read_bytes() == b"old!"
                assert state(root)["files"]["file.c"]["remote_mtime"] == "2026-10-05T10:00:00Z"
            else:
                assert (root / "file.c").read_bytes() == b"local edits"
                assert not (root / ".ferry/state.json").exists()
            print("PASS: init pull / " + mode)
        finally:
            peer.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    run(str(Path(args.binary).resolve()))
