//! Real exec binary -> embedded AppServer -> Rust cold/warm collector -> fake
//! native Node child -> real printf -> owned JSON output. No provider is called.
#![cfg(unix)]

use std::time::Duration;

use core_test_support::test_codex_exec::test_codex_exec;
use serde_json::Value;

const NATIVE_CJS: &str = r#"
const fs = require('fs');
const {spawnSync} = require('child_process');
const warm = process.argv.includes('--stdio');
// Bound even an orphaned fake with leaked stdin; no detached/background effects exist.
setTimeout(() => process.exit(74), 20000).unref();
fs.appendFileSync(process.env.NATIVE_TEST_INVOCATIONS, warm ? 'warm\n' : 'cold\n');
const modern = process.env.NATIVE_TEST_SCHEMA === 'modern';
const command = process.env.NATIVE_TEST_COMMAND;
const sessionId = 'native-json-session';
const turnId = 'native-json-turn';
let seq = 0;
let calls = 0;
const line = value => process.stdout.write(JSON.stringify(value) + '\n');
const emit = value => warm ? line({method:'session/event',params:{...value,seq:++seq}}) : line(value);
function tool(kind,payload) {
  emit({sessionId,turnId,type:modern?'tool.updated':'tool_call_'+kind,
    payload:{...payload,...(modern?{kind}: {})}});
}
function run() {
  if (++calls !== 1) throw new Error('unexpected additional native execution');
  tool('scheduled',{toolCallId:'native-json-call',toolName:'Bash',input:{command}});
  // Also expose the old model tool envelope: RELAY=false must still hold.
  emit({sessionId,turnId,type:'model.streaming',payload:{kind:'tool_call',toolCallId:'native-json-call',toolName:'Bash',input:{command}}});
  // This is the fixture's sole business effect. Relay would append twice.
  const actual = spawnSync('/bin/sh',['-c',command],{encoding:'utf8',timeout:1000});
  if (actual.error || actual.signal || actual.status !== 0) throw new Error('fixture printf failed');
  const content = actual.stdout.trimEnd(); // The documented native Bash returned-text formatter.
  const bytes = Buffer.byteLength(content,'utf8');
  tool('result',{toolCallId:'native-json-call',result:{success:true,content,truncated:false,
    originalBytes:bytes,returnedBytes:bytes,perf:{detail:{kind:'command',command:{
      exitCode:actual.status,status:'completed',timedOut:false}}}}});
  emit({sessionId,turnId,type:'model.streaming',payload:{kind:'text_delta',delta:'assistant quoted wrong-nonce'}});
  if (warm) emit({sessionId,turnId,type:'turn.completed',payload:{response:'done',resultType:'success',
    usage:{inputTokens:1,outputTokens:1,totalTokens:2}}});
  else emit({type:'result',response:'done'});
}
if (!warm) {
  emit({type:'session.updated',sessionId});
  run();
} else {
  require('readline').createInterface({input:process.stdin}).on('line',raw => {
    const message=JSON.parse(raw);
    const reply=result=>line({id:message.id,result});
    switch(message.method) {
      case 'session/create': reply({session:{sessionId},protocol:{name:'ZCode Protocol',version:1}}); break;
      case 'session/subscribe': reply({sessionId,eventSeq:seq,events:[]}); break;
      case 'session/send': run();reply({accepted:true,sessionId,stateRevision:1});break;
      case 'session/stop': reply({stopped:true}); break;
      default: throw new Error('unexpected native request method');
    }
  });
}
"#;

fn run_probe(warm: bool, modern: bool) -> anyhow::Result<()> {
    let test = test_codex_exec();
    let script = test.cwd_path().join("native-fixture.cjs");
    let effects = test.cwd_path().join("native-effects");
    let invocations = test.cwd_path().join("native-invocations");
    let tmp = test.cwd_path().join("tmp");
    std::fs::create_dir(&tmp)?;
    std::fs::write(&script, NATIVE_CJS)?;
    let quoted = format!("'{}'", effects.to_string_lossy().replace('\'', "'\\''"));
    let command =
        format!("printf '%s\\n' 'actual-pipeline-nonce'; printf '%s\\n' 'once' >> {quoted}");
    let mut cmd = test.cmd();
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", test.home_path())
        .env("CODEX_HOME", test.home_path())
        .env("CODEX_SQLITE_HOME", test.home_path())
        .env("TMPDIR", &tmp)
        .env("ZCODE_NODE", "node")
        .env("ZCODE_CJS", &script)
        .env("ZCODE_WARM", if warm { "1" } else { "0" })
        .env("ZCODE_WARM_GOAL", "0")
        .env("ZCODE_WARM_V4_SEND", "0")
        .env("NATIVE_TEST_SCHEMA", if modern { "modern" } else { "legacy" })
        .env("NATIVE_TEST_COMMAND", &command)
        .env("NATIVE_TEST_INVOCATIONS", &invocations)
        .timeout(Duration::from_secs(30))
        .args([
            "--skip-git-repo-check",
            "--json",
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            "model_provider=\"native_fixture\"",
            "-c",
            "model_providers.native_fixture={name=\"Synthetic native fixture\",wire_api=\"zcode\",base_url=\"http://127.0.0.1:9\",requires_openai_auth=false,supports_websockets=false}",
            "-m",
            "gpt-5.1",
            "Run the synthetic native printf fixture once.",
        ]);
    let output = cmd.output()?;
    assert!(
        output.status.success(),
        "synthetic exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let native: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.pointer("/item/type").and_then(Value::as_str) == Some("native_tool_result")
        })
        .collect();
    assert_eq!(
        native.len(),
        1,
        "must project the real matched return exactly once"
    );
    let item = &native[0]["item"];
    let result = &item["observation"];
    assert_eq!(native[0]["type"], "item.completed");
    assert_eq!(result["kind"], "result");
    assert_eq!(result["native_event"], "tool_call_result");
    assert_eq!(result["native_session_id"], "native-json-session");
    assert_eq!(result["native_turn_id"], "native-json-turn");
    assert_eq!(result["call_id"], "native-json-call");
    assert_eq!(result["tool"], "Bash");
    assert_eq!(result["command"], command);
    assert_eq!(result["content"], "actual-pipeline-nonce");
    assert_eq!(result["native_success"], true);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["status"], "completed");
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["native_truncated"], false);
    assert_eq!(result["local_truncated"], false);
    assert_eq!(result["observation_complete"], true);
    assert_eq!(
        result["native_returned_bytes"],
        "actual-pipeline-nonce".len()
    );
    let thread = rows
        .iter()
        .find(|row| row["type"] == "thread.started")
        .expect("real thread header");
    assert_eq!(item["thread_id"], thread["thread_id"]);
    assert!(item["turn_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(rows.iter().any(|row| row["type"] == "turn.completed"));
    assert!(
        !rows
            .iter()
            .any(|row| row.pointer("/item/type").and_then(Value::as_str)
                == Some("command_execution")),
        "native call must never become an execution request"
    );
    assert_eq!(
        std::fs::read_to_string(effects)?,
        "once\n",
        "native printf must run exactly once"
    );
    assert_eq!(
        std::fs::read_to_string(invocations)?,
        if warm { "warm\n" } else { "cold\n" },
        "warm test may not pass by falling back to cold"
    );
    Ok(())
}

#[test]
fn cold_modern_native_printf_reaches_owned_exec_json_once() -> anyhow::Result<()> {
    run_probe(/*warm*/ false, /*modern*/ true)
}
#[test]
fn cold_legacy_native_printf_reaches_owned_exec_json_once() -> anyhow::Result<()> {
    run_probe(/*warm*/ false, /*modern*/ false)
}
#[test]
fn warm_modern_native_printf_reaches_owned_exec_json_once() -> anyhow::Result<()> {
    run_probe(/*warm*/ true, /*modern*/ true)
}
#[test]
fn warm_legacy_native_printf_reaches_owned_exec_json_once() -> anyhow::Result<()> {
    run_probe(/*warm*/ true, /*modern*/ false)
}
