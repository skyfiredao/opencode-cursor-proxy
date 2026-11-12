#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import signal
import sqlite3
from pathlib import Path
import subprocess
import sys
import threading
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor

output_lock = threading.Lock()


def send(body):
    with output_lock:
        print(json.dumps({"jsonrpc": "2.0", **body}), flush=True)


def update(session, event, **fields):
    send({"method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": event, **fields},
    }})


parser = argparse.ArgumentParser()
parser.add_argument("--scenario", default=Path("acp-scenario").read_text() if Path("acp-scenario").exists() else "text")
parser.add_argument("--legacy")
args, _ = parser.parse_known_args()
legacy = Path(os.environ.get("CURSOR_PROXY_FIXTURE_LEGACY", str(Path(sys.argv[0]).parent / "legacy-agent")))
if args.legacy is None and legacy.exists():
    args.legacy = str(legacy.resolve())
if args.legacy and 'acp' not in sys.argv:
    os.execv('/bin/sh', ['sh',args.legacy,*sys.argv[1:]])
if '--list-models' in sys.argv:
    print('composer-2.5 - Composer 2.5')
    sys.exit(0)
session = str(uuid.uuid4()) if args.scenario.startswith("usage-store") else "session-" + str(os.getpid())
pid_file = Path(f"acp-pid.pending-{os.getpid()}")
pid_file.write_text(str(os.getpid()))
pid_file.replace("acp-pid")
pending = None
servers = []
memory = ""
mode = "agent"
mode_sets = 0
prompt_count = 0
baseline_lock = None


def store_context(used, capacity, nonce):
    def integer(value):
        result = bytearray()
        while value >= 128:
            result.append((value & 127) | 128)
            value >>= 7
        result.append(value)
        return bytes(result)
    details = b"\x08" + integer(used) + b"\x10" + integer(capacity)
    data = b"\x2a" + integer(len(details)) + details + b"\xf8\x07" + integer(nonce)
    directory = Path(os.environ["CURSOR_CONFIG_DIR"]) / "acp-sessions" / session
    directory.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(directory / "store.db") as db:
        db.executescript("PRAGMA user_version=1; CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE IF NOT EXISTS blobs(id TEXT PRIMARY KEY,data BLOB);")
        root = hashlib.sha256(data).hexdigest()
        db.execute("INSERT OR REPLACE INTO blobs VALUES(?,?)", (root,data))
        db.execute("INSERT OR REPLACE INTO meta VALUES('0',?)", (json.dumps({"latestRootBlobId":root,"agentId":session}).encode().hex(),))


def legacy_turn(request_id, prompt):
    arguments = [value for value in sys.argv[sys.argv.index(args.legacy) + 1:] if value != "acp"] if args.legacy in sys.argv else [value for value in sys.argv[1:] if value != "acp"]
    if mode == "plan":
        arguments += ["--mode", "plan"]
    completed = subprocess.run(['/bin/sh', args.legacy, *arguments], input=prompt, text=True, capture_output=True)
    if completed.returncode:
        send({"id":request_id,"error":{"code":-32603,"message":completed.stderr or "mock exited"}})
        return
    accumulated = ""
    for line in completed.stdout.splitlines():
        event = json.loads(line)
        if event["type"] == "thinking" and event.get("subtype") == "delta":
            update(session, "agent_thought_chunk", content={"type":"text","text":event["text"]})
        if event["type"] == "tool_call" and event.get("subtype") != "completed" and servers:
            listed = mcp_request("tools/list", {})
            name = listed["result"]["tools"][0]["name"]
            raw = event["tool_call"]["readToolCall"]["args"]
            parameters = {"filePath":raw["path"]}
            mcp_request("tools/call", {"name":name,"arguments":parameters})
        if event["type"] == "assistant" and "timestamp_ms" in event and "model_call_id" not in event:
            text = "".join(part.get("text", "") for part in event["message"]["content"])
            if text.startswith('{"name"') and servers:
                envelope = json.loads(text)
                mcp_request("tools/call", {"name":envelope["name"],"arguments":envelope["arguments"]})
            else:
                accumulated += text
                update(session, "agent_message_chunk", content={"type":"text","text":text})
        if event["type"] == "result":
            if event.get("is_error"):
                send({"id":request_id,"error":{"code":-32603,"message":event.get("result","failure")}})
                return
            if not accumulated and event.get("result"):
                update(session, "agent_message_chunk", content={"type":"text","text":event["result"]})
    send({"id":request_id,"result":{"stopReason":"end_turn"}})


def mcp_request(method, params):
    configuration = servers[0]
    headers = {header["name"]: header["value"] for header in configuration["headers"]}
    headers.update({"Content-Type":"application/json", "Accept":"application/json, text/event-stream", "MCP-Protocol-Version":"2025-11-25"})
    body = json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode()
    with urllib.request.urlopen(urllib.request.Request(configuration["url"], data=body, headers=headers), timeout=10) as response:
        return json.load(response)


def run_legacy_turn(request_id, prompt):
    try:
        legacy_turn(request_id, prompt)
    except Exception as error:
        send({"id":request_id,"error":{"code":-32603,"message":str(error)}})


def invalid_mcp_turn(request_id):
    mcp_request("initialize", {"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fake-cursor","version":"1"}})
    result = mcp_request("tools/call", {"name":"host_echo","arguments":{"text":42}})
    if not result["result"]["isError"]:
        raise RuntimeError("invalid MCP arguments were accepted")
    update(session, "agent_thought_chunk", content={"type":"text","text":"THOUGHT_ONLY"})
    send({"id":request_id,"result":{"stopReason":"end_turn"}})


def host_turn(request_id):
    configuration = servers[0]
    headers = {header["name"]: header["value"] for header in configuration["headers"]}
    headers.update({"Content-Type":"application/json", "Accept":"application/json, text/event-stream", "MCP-Protocol-Version":"2025-11-25"})
    sequence = 0
    requests_lock = threading.Lock()
    def request(method, params):
        nonlocal sequence
        with requests_lock:
            sequence += 1
            request_id = sequence
        body = json.dumps({"jsonrpc":"2.0","id":request_id,"method":method,"params":params}).encode()
        with urllib.request.urlopen(urllib.request.Request(configuration["url"], data=body, headers=headers), timeout=10) as response:
            return json.load(response)
    request("initialize", {"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fake-cursor","version":"1"}})
    listed = request("tools/list", {})
    name = listed["result"]["tools"][0]["name"]
    if args.scenario == "usage-host-tool":
        update(session, "usage_update", used=12167, size=200000)
    update(session, "tool_call", toolCallId="mcp-status", kind="other", status="pending", rawInput={})
    if args.scenario == "ordered-tools":
        for _ in range(120):
            update(session, "agent_message_chunk", content={"type":"text","text":"x"})
    if args.scenario == "parallel-tools":
        with ThreadPoolExecutor(max_workers=2) as workers:
            results = list(workers.map(lambda text: request("tools/call", {"name":name,"arguments":{"text":text}}), ["hello-1", "hello-2"]))
        text = "|".join(result["result"]["content"][0]["text"] for result in results)
    else:
        result = request("tools/call", {"name":name,"arguments":{"text":"hello"}})
        text = result["result"]["content"][0]["text"]
    update(session, "tool_call_update", toolCallId="mcp-status", status="completed")
    update(session, "agent_message_chunk", content={"type":"text","text":text})
    send({"id":request_id,"result":{"stopReason":"end_turn"}})


def run_host_turn(request_id):
    try:
        host_turn(request_id)
    except Exception as error:
        send({"id":request_id,"error":{"code":-32603,"message":str(error)}})


def policy_catalog_turn(request_id, prompt):
    names = []
    if servers:
        mcp_request("initialize", {"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fake-cursor","version":"1"}})
        names = [tool["name"] for tool in mcp_request("tools/list", {})["result"]["tools"]]
    update(session, "agent_message_chunk", content={"type":"text","text":json.dumps(names)})
    tool = "read" if "CALL_READ" in prompt else "write" if "CALL_WRITE" in prompt else None
    if tool is not None:
        result = mcp_request("tools/call", {"name":tool,"arguments":{}})
        if "result" in result:
            update(session, "agent_message_chunk", content={"type":"text","text":result["result"]["content"][0]["text"]})
    send({"id":request_id,"result":{"stopReason":"end_turn"}})


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params", {})
    if method == "initialize":
        if args.scenario == "hang-init":
            continue
        send({"id": request_id, "result": {"protocolVersion": 1,
              "agentCapabilities": {"loadSession":True,"mcpCapabilities": {"http": True}}, "authMethods": []}})
    elif method == "session/new":
        servers = params.get("mcpServers", [])
        if args.scenario.startswith("usage-store") and args.scenario != "usage-store-missing":
            store_context(1000, 100000, 0)
            if args.scenario == "usage-store-baseline-locked":
                baseline_lock = sqlite3.connect(Path(os.environ["CURSOR_CONFIG_DIR"]) / "acp-sessions" / session / "store.db")
                baseline_lock.execute("BEGIN EXCLUSIVE")
        if args.scenario == "overflow":
            for _ in range(420):
                update(session, "agent_message_chunk", content={"type": "text", "text": "z" * 40000})
        if args.scenario == "early":
            update(session, "agent_message_chunk", content={"type": "text", "text": "EARLY"})
        created = {"sessionId": session, "configOptions":[{
            "id":"mode","category":"mode","name":"Mode","type":"select","currentValue":"agent",
            "options":[{"value":"agent","name":"Agent"},{"value":"plan","name":"Plan"}],
        }]}
        if args.scenario.startswith("mode-"):
            created["modes"] = {"currentModeId":"agent","availableModes":[{"id":"agent","name":"Agent"},{"id":"plan","name":"Plan"}]}
            if "legacy" in args.scenario:
                del created["configOptions"]
            if "missing-plan" in args.scenario or "missing-agent" in args.scenario:
                missing = "plan" if "missing-plan" in args.scenario else "agent"
                if "configOptions" in created:
                    created["configOptions"][0]["options"] = [option for option in created["configOptions"][0]["options"] if option["value"] != missing]
                else:
                    created["modes"]["availableModes"] = [option for option in created["modes"]["availableModes"] if option["id"] != missing]
            if args.scenario == "mode-none":
                created = {"sessionId":session}
            if "grouped" in args.scenario:
                options = created["configOptions"][0]["options"]
                created["configOptions"][0]["options"] = [{"group":"modes","name":"Modes","options":options}]
        send({"id": request_id,"result": created})
        if args.scenario == "barrier":
            update(session, "tool_call_update", toolCallId="grant-1", rawInput={"providerIdentifier":"bridge","toolName":"probe","args":{}})
            send({"id":901,"method":"session/request_permission","params":{
                "sessionId":session,"toolCall":{"toolCallId":"grant-1"},
                "options":[{"optionId":"deny-7","kind":"reject_once"},{"optionId":"allow-7","kind":"allow_once"}],
            }})
        if args.scenario == "blocked-input":
            signal.pause()
    elif method == "session/prompt":
        pending = request_id
        prompt_count += 1
        if args.scenario.startswith("usage-store"):
            if baseline_lock is not None:
                baseline_lock.rollback()
                baseline_lock.close()
                baseline_lock = None
            prompt_text = "".join(block.get("text", "") for block in params["prompt"])
            if args.scenario not in {"usage-store-stale", "usage-store-missing", "usage-store-baseline-locked"} and not (args.scenario == "usage-store-stale-second" and "PONG_AGAIN" in prompt_text):
                store_context(12167, 100000, prompt_count)
            if args.scenario == "usage-store-corrupt":
                with sqlite3.connect(Path(os.environ["CURSOR_CONFIG_DIR"]) / "acp-sessions" / session / "store.db") as db:
                    db.execute("UPDATE meta SET value='invalid' WHERE key='0'")
            update(session, "agent_message_chunk", content={"type":"text","text":"PONG"})
            send({"id":request_id,"result":{"stopReason":"end_turn"}})
            continue
        if args.scenario in {"usage-reported", "usage-context", "usage-reported-once", "usage-context-once"}:
            prompt_text = "".join(block.get("text", "") for block in params["prompt"])
            publish = not args.scenario.endswith("-once") or "PONG_AGAIN" not in prompt_text
            if publish:
                update(session, "usage_update", used=12167, size=200000)
            if args.scenario == "usage-context":
                update("another-session", "usage_update", used=99999, size=200000)
            update(session, "agent_message_chunk", content={"type":"text","text":"PONG"})
            result = {"stopReason":"end_turn"}
            if args.scenario.startswith("usage-reported") and publish:
                result["usage"] = json.loads(Path("usage-report.json").read_text())
            send({"id":request_id,"result":result})
            continue
        if args.scenario.startswith("mode-"):
            with Path("mode-trace.ndjson").open("a") as trace:
                trace.write(json.dumps({"method":method,"params":params}) + "\n")
            update(session, "agent_message_chunk", content={"type":"text","text":"MODE_OK"})
            send({"id":request_id,"result":{"stopReason":"end_turn"}})
            continue
        if args.scenario == "policy-catalog":
            threading.Thread(target=policy_catalog_turn, args=(request_id,"".join(block.get("text", "") for block in params["prompt"])),daemon=True).start()
            continue
        if args.scenario.startswith("repair-"):
            text = "".join(block.get("text", "") for block in params["prompt"])
            with Path("repair-prompts.ndjson").open("a") as log:
                log.write(json.dumps({"sessionId":params["sessionId"],"text":text}) + "\n")
            if args.scenario == "repair-reported":
                if prompt_count == 1:
                    update(session, "agent_thought_chunk", content={"type":"text","text":"THOUGHT_ONLY"})
                    send({"id":request_id,"result":{"stopReason":"end_turn","usage":json.loads(Path("usage-report.json").read_text())}})
                else:
                    update(session, "usage_update", used=12167, size=200000)
                    update(session, "agent_message_chunk", content={"type":"text","text":"RECOVERED"})
                    result = {"stopReason":"end_turn"}
                    if Path("recovery-usage.json").exists():
                        result["usage"] = json.loads(Path("recovery-usage.json").read_text())
                    send({"id":request_id,"result":result})
                continue
            if args.scenario == "repair-mcp-invalid" or (args.scenario == "repair-second-mcp-invalid" and prompt_count == 2):
                invalid_mcp_turn(request_id)
                continue
            if prompt_count == 1 or args.scenario == "repair-repeated" or (args.scenario == "repair-per-turn" and prompt_count % 2 == 1):
                if args.scenario != "repair-empty":
                    update(session, "agent_thought_chunk", content={"type":"text","text":"THOUGHT_ONLY"})
                if args.scenario == "repair-native":
                    update(session, "tool_call", toolCallId="native-once", kind="edit", status="completed")
                if args.scenario == "repair-permission":
                    send({"id":903,"method":"session/request_permission","params":{
                        "sessionId":session,"toolCall":{"toolCallId":"native-permission"},
                        "options":[{"optionId":"deny","kind":"reject_once"}],
                    }})
                if args.scenario == "repair-task":
                    send({"method":"cursor/task","params":{"toolCallId":"native-task"}})
                if args.scenario == "repair-timeout":
                    continue
                if args.scenario == "repair-delayed-second-timeout":
                    threading.Event().wait(0.4)
                if args.scenario == "repair-error":
                    send({"id":request_id,"error":{"code":-32603,"message":"controlled upstream error"}})
                elif args.scenario == "repair-normal":
                    update(session, "agent_message_chunk", content={"type":"text","text":"NORMAL"})
                    send({"id":request_id,"result":{"stopReason":"end_turn"}})
                else:
                    send({"id":request_id,"result":{"stopReason":"cancelled" if args.scenario == "repair-cancelled" else "end_turn"}})
            elif args.scenario == "repair-second-native":
                update(session, "tool_call_update", toolCallId="unexpected-native", status="completed")
                send({"id":request_id,"result":{"stopReason":"end_turn"}})
            elif args.scenario in {"repair-second-timeout", "repair-delayed-second-timeout"}:
                update(session, "agent_thought_chunk", content={"type":"text","text":"STILL_THINKING"})
            else:
                update(session, "agent_message_chunk", content={"type":"text","text":"RECOVERED"})
                send({"id":request_id,"result":{"stopReason":"end_turn"}})
            continue
        if args.legacy:
            Path("received-session").write_text(session)
        if args.legacy:
            threading.Thread(target=run_legacy_turn, args=(request_id,"".join(block.get("text", "") for block in params["prompt"])),daemon=True).start()
            continue
        if args.scenario in {"memory", "native-status"}:
            text = "".join(block.get("text", "") for block in params["prompt"])
            if "REMEMBER:" in text:
                memory = text.split("REMEMBER:", 1)[1].split()[0]
            if args.scenario == "native-status":
                update(session, "tool_call", toolCallId="native-call", kind="read", status="pending")
                update(session, "tool_call_update", toolCallId="native-call", status="completed")
            update(session, "agent_message_chunk", content={"type":"text","text":session + ":" + memory})
            send({"id":request_id,"result":{"stopReason":"end_turn"}})
            continue
        if args.scenario in {"host-tool", "parallel-tools", "ordered-tools", "late-tool-text", "blocked-barrier", "usage-host-tool"}:
            threading.Thread(target=run_host_turn, args=(request_id,), daemon=True).start()
            continue
        if args.scenario == "cancel":
            continue
        if args.scenario == "exit":
            update(session, "agent_message_chunk", content={"type": "text", "text": "LAST"})
            print("controlled ACP exit", file=sys.stderr, flush=True)
            subprocess.Popen(["sleep", "30"])
            sys.exit(7)
        if args.scenario == "question":
            send({"id": 900, "method": "cursor/ask_question", "params": {"questions": []}})
            continue
        if args.scenario == "flood":
            for _ in range(100):
                update(session, "agent_message_chunk", content={"type":"text","text":"flood"})
            continue
        update(session, "agent_thought_chunk", content={"type": "text", "text": "THOUGHT"})
        for text in ["ha", "ha", "你好"]:
            update(session, "agent_message_chunk", content={"type": "text", "text": text})
        send({"id": request_id, "result": {"stopReason": "end_turn"}})
    elif method == "session/cancel":
        if pending is not None:
            send({"id": pending, "result": {"stopReason": "cancelled"}})
    elif method in {"session/set_mode", "session/set_config_option"}:
        mode_sets += 1
        if args.scenario == "usage-host-tool" and mode_sets in {2,3}:
            update(session, "usage_update", used=12168 if mode_sets == 2 else 16000, size=200000)
        if args.scenario.startswith("mode-"):
            with Path("mode-trace.ndjson").open("a") as trace:
                trace.write(json.dumps({"method":method,"params":params}) + "\n")
            if "rejected" in args.scenario:
                send({"id":request_id,"error":{"code":-32602,"message":"mode rejected"}})
                continue
        if args.scenario == "blocked-barrier" and mode_sets == 2:
            Path("barrier-reached").touch()
            continue
        mode = params.get("modeId", params.get("value", "agent"))
        send({"id":request_id,"result":{}})
        if args.scenario == "late-tool-text" and mode_sets == 2:
            update(session, "agent_message_chunk", content={"type":"text","text":"LATE_PRE_TOOL"})
    elif method == "probe/grant":
        update(session, "tool_call_update", toolCallId=params.get("toolId", "grant-1"), rawInput={"providerIdentifier":"bridge","toolName":params.get("toolName", "probe"),"args":{}})
        send({"id":request_id,"result":{}})
    elif method == "probe/permission":
        pending = request_id
        send({"id":902,"method":"session/request_permission","params":{
            "sessionId":params.get("sessionId",session),"toolCall":{"toolCallId":"grant-1"},
            "options":[{"optionId":"deny-7","kind":"reject_once"},{"optionId":"allow-7","kind":"allow_once"}],
        }})
    elif request_id == 901 and "result" in message:
        published = Path("permission-result.pending")
        published.write_text(json.dumps(message["result"]))
        published.replace("permission-result")
    elif request_id == 902 and "result" in message:
        send({"id":pending,"result":message["result"]})
    elif request_id == 900 and "result" in message:
        if message["result"]["outcome"]["outcome"] != "skipped":
            sys.exit(9)
        update(session, "agent_message_chunk", content={"type": "text", "text": "SKIPPED"})
        send({"id": pending, "result": {"stopReason": "end_turn"}})
    elif request_id is not None:
        send({"id": request_id, "result": {}})
