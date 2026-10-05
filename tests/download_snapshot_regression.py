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
        self.listed = []
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
        rename_from = None
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
                        payload = self.file(path)
                        reply(f"213 {len(payload)}" if payload is not None else "550 not a file")
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
                    elif cmd in ("LIST", "RETR", "STOR"):
                        if cmd == "RETR" and self.file(path) is None:
                            reply("550 missing")
                            continue
                        reply("150 opening data connection")
                        data, _ = passive.accept()
                        with data:
                            if cmd == "STOR":
                                chunks = []
                                while chunk := data.recv(65536):
                                    chunks.append(chunk)
                                self.store(path, b"".join(chunks))
                                payload = None
                            elif cmd == "LIST":
                                self.listed.append(path)
                                payload = self.listing(path)
                            else:
                                payload = self.file(path)
                            if payload is not None:
                                data.sendall(payload)
                        passive.close()
                        passive = None
                        if cmd == "RETR" and (self.mode == "during_transfer" or
                                              self.mode == "during_init_transfer" and self.events.count("RETR") == 2):
                            self.body, self.mtime = b"new!", NEW_TIME
                        reply("226 transfer complete")
                    elif cmd == "RNFR":
                        rename_from = path
                        reply("350 ready for destination")
                    elif cmd == "RNTO":
                        self.rename(rename_from, path)
                        reply("250 renamed")
                    elif cmd == "QUIT":
                        reply("221 bye")
                        break
                    else:
                        raise AssertionError(f"Unexpected FTP operation: {cmd} {arg}")
        finally:
            if passive:
                passive.close()

    def store(self, path, payload):
        raise AssertionError("Unexpected upload")

    def rename(self, source, target):
        raise AssertionError("Unexpected rename")

    def file(self, path):
        return self.body if path == "/file.c" else None

    def listing(self, path):
        return f"-rw-r--r-- 1 test test {len(self.body)} Oct 05 10:00 file.c\r\n".encode()

    def close(self):
        self.stop.set()
        self.thread.join(timeout=6)
        self.listener.close()
        assert not self.thread.is_alive(), "FTP peer failed to stop"
        assert self.failure is None, self.failure
        if self.root is not None:
            shutil.rmtree(self.root)


class TreePeer(Peer):
    def __init__(self, tree):
        self.tree = tree
        self.overrides, self.uploads = {}, []
        super().__init__()

    def store(self, path, payload):
        assert path.startswith("/ferry-tmp."), path
        self.overrides[path] = payload
        self.uploads.append(path)
        self.mtime = NEW_TIME

    def rename(self, source, target):
        self.overrides[target] = self.overrides.pop(source)
        parent, _, leaf = target.rpartition("/")
        self.tree[parent or "/"][leaf] = False

    def file(self, path):
        if path in self.overrides:
            return self.overrides[path]
        parent, _, leaf = path.rpartition("/")
        children = self.tree.get(parent or "/", {})
        return self.body if leaf in children and not children[leaf] else None

    def listing(self, path):
        return "".join(
            f"{'drwxr-xr-x' if is_dir else '-rw-r--r--'} 1 test test {len(self.body)} Oct 05 10:00 {name}\r\n"
            for name, is_dir in self.tree[path].items()
        ).encode()


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

    for ignores, expected_lists in ((["ignored/"], ["/"]),
                                   (["ignored/", "!ignored/deep/keep.c"], ["/", "/ignored", "/ignored/deep"])):
        peer = TreePeer({"/": {"file.c": False, "ignored": True},
                         "/ignored": {"deep": True, "skip.c": False},
                         "/ignored/deep": {"keep.c": False, "skip.c": False}})
        root = Path(tempfile.mkdtemp(prefix="ferry-push-pruning-test-"))
        peer.root = root
        try:
            config(root, peer)
            cfg = root / ".ferry.toml"
            cfg.write_text(cfg.read_text().replace('ignore=[".ferry/", ".ferry.toml"]',
                                                   'ignore=' + json.dumps([".ferry/", ".ferry.toml", *ignores])))
            (root / "file.c").write_bytes(b"old!")
            result = invoke(binary, root, "--dry-run", "push", success=False)
            assert "conflict (Untracked" in result.stderr, result.stderr
            assert peer.listed == expected_lists, peer.listed
            peer.listed.clear()
            invoke(binary, root, "--dry-run", "push", "--force")
            assert peer.listed == expected_lists, peer.listed
            assert not (root / ".ferry/state.json").exists()
            assert (root / "file.c").read_bytes() == b"old!"
            (root / "file.c").write_bytes(b"new local contents")
            invoke(binary, root, "push", success=False)
            assert not peer.uploads, "conflict must never upload"
            assert not state(root)["files"]
            invoke(binary, root, "push", "--force")
            assert len(peer.uploads) == 1
            assert peer.file("/file.c") == b"new local contents"
            assert peer.file("/ignored/deep/keep.c") == b"old!"
            assert peer.file("/ignored/skip.c") == b"old!"
            assert set(state(root)["files"]) == {"file.c"}
            print("PASS: push pruning, conflict, dry-run, forced upload / " + repr(ignores))
        finally:
            peer.close()

    for batched, expected_connections in ((False, 2), (True, 1)):
        peer = TreePeer({"/": {"file.c": False, "second.c": False}})
        root = Path(tempfile.mkdtemp(prefix="ferry-batch-test-"))
        peer.root = root
        try:
            config(root, peer)
            if batched:
                invoke(binary, root, "pull", "file.c", "second.c")
            else:
                invoke(binary, root, "pull", "file.c")
                invoke(binary, root, "pull", "second.c")
            assert peer.events.count("USER") == expected_connections
            assert peer.events.count("RETR") == 2
            for name in ("file.c", "second.c"):
                assert (root / name).read_bytes() == b"old!"
            assert set(state(root)["files"]) == {"file.c", "second.c"}
            print(f"PASS: {'batched' if batched else 'separate'} pulls / {expected_connections} connections")
        finally:
            peer.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    run(str(Path(args.binary).resolve()))
