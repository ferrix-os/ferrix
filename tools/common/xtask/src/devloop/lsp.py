# test-devloop's language-server session (docs/DEV-ON-FERRIX.md, D1): what
# an editor asks rust-analyzer first, on the fixture, with nothing but the
# standard library. Usage: python3 lsp.py SERVER WORKSPACE FILE
#
# It says one `lsp:` line a step and exits 0 when the server answered
# `initialize`, gave the file's symbols, found `greeting` in the workspace and
# shut down; anything else ends it with the step that failed and status 1.
import json
import os
import select
import subprocess
import sys
import time

server, root, path = sys.argv[1:4]
child = subprocess.Popen([server], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=open("/tmp/rust-analyzer.err", "wb"))
pending = b""
started = time.time()


def fail(step):
    print("lsp: FAILED at %s (server status %s)" % (step, child.poll()), flush=True)
    child.kill()
    sys.exit(1)


def send(message):
    body = json.dumps(dict(message, jsonrpc="2.0")).encode()
    try:
        child.stdin.write(b"Content-Length: %d\r\n\r\n%s" % (len(body), body))
        child.stdin.flush()
    except BrokenPipeError:
        fail("a write to the server")


def receive(seconds):
    global pending
    deadline = time.time() + seconds
    while True:
        end = pending.find(b"\r\n\r\n")
        if end >= 0:
            length = next(int(line.split(b":")[1]) for line in pending[:end].split(b"\r\n")
                          if line.lower().startswith(b"content-length"))
            if len(pending) >= end + 4 + length:
                message = json.loads(pending[end + 4:end + 4 + length])
                pending = pending[end + 4 + length:]
                return message
        left = deadline - time.time()
        if left <= 0:
            return None
        ready, _, _ = select.select([child.stdout], [], [], min(left, 1))
        if ready:
            data = os.read(child.stdout.fileno(), 65536)
            if not data:
                return None
            pending += data


def answer(request_id, step, seconds):
    """The reply to request_id; the server's own requests are answered null."""
    while True:
        message = receive(seconds)
        if message is None:
            fail(step)
        if "method" in message and "id" in message:
            send({"id": message["id"], "result": None})
        elif message.get("id") == request_id:
            return message


send({"id": 1, "method": "initialize", "params": {
    "processId": os.getpid(), "rootUri": "file://" + root, "capabilities": {}}})
reply = answer(1, "initialize", 120)
print("lsp: initialized %s" % reply["result"]["serverInfo"]["name"], flush=True)
send({"method": "initialized", "params": {}})
uri = "file://" + path
send({"method": "textDocument/didOpen", "params": {"textDocument": {
    "uri": uri, "languageId": "rust", "version": 1, "text": open(path).read()}}})
send({"id": 2, "method": "textDocument/documentSymbol", "params": {"textDocument": {"uri": uri}}})
symbols = [symbol["name"] for symbol in answer(2, "documentSymbol", 300).get("result") or []]
if "greeting" not in symbols:
    fail("documentSymbol: %s" % symbols)
print("lsp: the file's symbols include greeting", flush=True)
# The workspace is indexed in the background: ask until it answers.
found = []
while not found and time.time() < started + 400:
    send({"id": 3, "method": "workspace/symbol", "params": {"query": "greeting"}})
    found = [s["name"] for s in answer(3, "workspace/symbol", 120).get("result") or []]
    if not found:
        time.sleep(2)
if "greeting" not in found:
    fail("workspace/symbol: %s" % found)
print("lsp: the workspace's symbols include greeting after %d s" % (time.time() - started),
      flush=True)
send({"id": 4, "method": "shutdown", "params": None})
answer(4, "shutdown", 60)
send({"method": "exit", "params": None})
print("lsp: the server exited with %s" % child.wait(timeout=60), flush=True)
