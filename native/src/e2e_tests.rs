//! End-to-end test: the agent loop driving the real `CodeAssistClient` against an
//! axum mock Code Assist server that streams SSE (text, then a `read_file` call,
//! then a final answer on the second request). No real credentials are used.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::history::History;
use crate::agent::tools::{ToolLimits, ToolRegistry, Workspace};
use crate::agent::{Agent, AgentConfig, AgentEvent, FinishReason, MAX_TURNS};
use crate::api::{ApiConfig, CodeAssistClient, Role};
use crate::auth::StaticToken;

#[derive(Clone, Default)]
struct Mock {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\r\n\r\n")).collect()
}

async fn stream(State(m): State<Mock>, Json(body): Json<Value>) -> String {
    m.bodies.lock().unwrap().push(body);
    let n = m.calls.fetch_add(1, Ordering::SeqCst);
    let chunk = |parts: Value| json!({"response":{"candidates":[{"content":{"role":"model","parts":parts}}]}});
    if n == 0 {
        sse(&[
            chunk(json!([{"text":"Let me read "}])),
            chunk(json!([{"text":"the file."}])),
            chunk(json!([{"functionCall":{"name":"read_file","args":{"file_path":"notes.txt"}}}])),
        ])
    } else {
        sse(&[
            chunk(json!([{"text":"The file says: "}])),
            json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hello e2e"}]},"finishReason":"STOP"}]}}),
        ])
    }
}

#[tokio::test]
async fn agent_reads_file_via_mock_code_assist() {
    let mock = Mock::default();
    let app = Router::new()
        .route(
            "/v1internal:loadCodeAssist",
            post(|| async { Json(json!({"currentTier":{"id":"free-tier"},"cloudaicompanionProject":"proj-e2e"})) }),
        )
        .route("/v1internal:streamGenerateContent", post(stream))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "hello e2e").unwrap();

    let cfg = ApiConfig { endpoint: format!("http://{addr}"), ..ApiConfig::default() };
    let client = CodeAssistClient::new(reqwest::Client::new(), Arc::new(StaticToken("test-token".into())), cfg);
    let tools = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), ToolLimits::default());
    let mut agent_cfg = AgentConfig::new("gemini-2.5-pro", "sys", "e2e");
    agent_cfg.max_turns = MAX_TURNS;
    let mut agent = Agent::new(Arc::new(client), tools, agent_cfg, History::new());

    let (tx, mut rx) = mpsc::unbounded_channel();
    agent.run_prompt("What is in notes.txt?".into(), tx, CancellationToken::new()).await;

    let mut text = String::new();
    let mut requests = Vec::new();
    let mut responses = Vec::new();
    let mut finished = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Content(t) => text.push_str(&t),
            AgentEvent::ToolCallRequest { name, args, .. } => requests.push((name, args)),
            AgentEvent::ToolCallResponse { name, ok, .. } => responses.push((name, ok)),
            AgentEvent::Confirmation(_) => panic!("read_file must be auto-approved"),
            AgentEvent::Error(e) => panic!("unexpected error: {e}"),
            AgentEvent::Finished { reason, history } => finished = Some((reason, history)),
            _ => {}
        }
    }

    assert_eq!(text, "Let me read the file.The file says: hello e2e");
    assert_eq!(requests, vec![("read_file".to_string(), json!({"file_path":"notes.txt"}))]);
    assert_eq!(responses, vec![("read_file".to_string(), true)]);

    let (reason, history) = finished.expect("Finished event");
    assert_eq!(reason, FinishReason::Completed);
    assert_eq!(history.len(), 4, "user, model(call), user(response), model(answer)");
    assert_eq!(history[0].role, Some(Role::User));
    assert_eq!(history[0].text(), "What is in notes.txt?");
    assert_eq!(history[1].role, Some(Role::Model));
    assert_eq!(history[1].text(), "Let me read the file.");
    assert_eq!(history[1].function_calls().next().expect("call").name, "read_file");
    let fr = history[2].parts.iter().find_map(|p| p.function_response.as_ref()).expect("functionResponse");
    assert_eq!(fr.name, "read_file");
    assert_eq!(fr.response["output"], "hello e2e");
    assert_eq!(history[3].text(), "The file says: hello e2e");

    // Wire checks: two stream requests with the onboarded project, and the second
    // carries the function response back to the model.
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    let bodies = mock.bodies.lock().unwrap();
    assert!(bodies.iter().all(|b| b["project"] == "proj-e2e" && b["model"] == "gemini-2.5-pro"));
    let second = bodies[1]["request"]["contents"].to_string();
    assert!(second.contains("functionResponse") && second.contains("hello e2e"));
}

// ---------------------------------------------------------------------------------------------
// Backend-level orchestration: planning prompt -> plan -> review -> execution -> walkthrough.

mod orchestration {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use crate::agent::artifacts::{ArtifactComment, ReviewState};
    use crate::agent::orchestrate::ReviewOutcome;
    use crate::agent::{AgentEvent, FinishReason};
    use crate::api::{ApiConfig, CodeAssistClient, Content, Role};
    use crate::app::{AppCommand, AppEvent, AppOptions, Backend};
    use crate::auth::StaticToken;
    use crate::settings::{AgentMode, ArtifactReviewPolicy, Settings, TerminalPolicy};
    use crate::store::ConversationStore;

    /// Mock Code Assist that answers the n-th stream request with `script[n]` (model parts).
    #[derive(Clone)]
    struct Scripted {
        script: Arc<Vec<Value>>,
        calls: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<Value>>>,
    }

    async fn stream(State(m): State<Scripted>, Json(body): Json<Value>) -> String {
        m.bodies.lock().unwrap().push(body);
        let n = m.calls.fetch_add(1, Ordering::SeqCst);
        let parts = m.script.get(n).cloned().unwrap_or_else(|| json!([{"text": "(script exhausted)"}]));
        let ev = json!({"response": {"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}]}});
        format!("data: {ev}\r\n\r\n")
    }

    async fn mock(script: Vec<Value>) -> (String, Scripted) {
        let m = Scripted { script: Arc::new(script), calls: Arc::default(), bodies: Arc::default() };
        let app = Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(|| async { Json(json!({"currentTier": {"id": "free-tier"}, "cloudaicompanionProject": "proj-e2e"})) }),
            )
            .route("/v1internal:streamGenerateContent", post(stream))
            .with_state(m.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), m)
    }

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"functionCall": {"id": id, "name": name, "args": args}})
    }

    /// Start a backend on `data` with `settings` and a client pointed at `endpoint`.
    fn start(
        data: &Path,
        settings: &Settings,
        endpoint: String,
    ) -> (mpsc::UnboundedSender<AppCommand>, mpsc::UnboundedReceiver<AppEvent>, CancellationToken) {
        settings.save(data).unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let opts = AppOptions { data_dir: data.to_path_buf(), workspace: None, model: None, mode: None, skip_permissions: false };
        let mut b = Backend::new(opts, shutdown.clone(), ev_tx, cmd_tx.clone());
        let cfg = ApiConfig { endpoint, ..ApiConfig::default() };
        b.set_client_for_test(CodeAssistClient::new(reqwest::Client::new(), Arc::new(StaticToken("test-token".into())), cfg));
        tokio::spawn(b.run(cmd_rx));
        (cmd_tx, ev_rx, shutdown)
    }

    /// Collect agent events of one turn until `Finished`; `on_review` answers reviews.
    /// Panics on any tool confirmation: these scenarios must run without one.
    async fn run_turn(
        rx: &mut mpsc::UnboundedReceiver<AppEvent>,
        mut on_review: impl FnMut(&[String]) -> ReviewOutcome,
    ) -> (Uuid, Vec<AgentEvent>) {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let ev = tokio::time::timeout_at(deadline, rx.recv()).await.expect("turn timed out").expect("backend gone");
            match ev {
                AppEvent::TurnRejected(e) => panic!("turn rejected: {e}"),
                AppEvent::TurnEvent { conversation, event } => {
                    let event = match event {
                        AgentEvent::ReviewRequested(req) => {
                            let outcome = on_review(&req.artifacts);
                            let _ = req.respond.send(outcome);
                            AgentEvent::Notify(format!("review:{}", req.artifacts.join(",")))
                        }
                        AgentEvent::Confirmation(req) => panic!("no confirmation expected, got {}", req.tool_name),
                        e => e,
                    };
                    let done = matches!(event, AgentEvent::Finished { .. });
                    seen.push(event);
                    if done {
                        return (conversation, seen);
                    }
                }
                _ => {}
            }
        }
    }

    /// Wait until the backend saved the conversation with `n` messages.
    async fn saved(data: &Path, id: Uuid, n: usize) -> crate::store::Conversation {
        let store = ConversationStore::new(data);
        for _ in 0..200 {
            if let Ok(c) = store.load(id) {
                if c.messages.len() == n {
                    return c;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("conversation {id} was not saved with {n} messages");
    }

    fn assert_alternates(h: &[Content]) {
        for (i, c) in h.iter().enumerate() {
            let want = if i % 2 == 0 { Role::User } else { Role::Model };
            assert_eq!(c.role, Some(want), "history[{i}] has the wrong role");
        }
    }

    const PLAN: &str = "# Implementation Plan\n\n- Change the greeting in notes.txt\n";
    const WALKTHROUGH: &str = "# Walkthrough\n\nnotes.txt now says hello planned world.\n";

    #[tokio::test]
    async fn planning_prompt_runs_plan_review_execute_walkthrough() {
        let script = vec![
            json!([
                {"text": "I will plan first."},
                call("t1", "task_boundary", json!({"task_name": "Plan the change", "summary": "Writing the plan", "phase": "planning"})),
                call("w1", "write_artifact", json!({"name": "implementation_plan", "content": PLAN, "summary": "One edit"})),
                call("r1", "request_review", json!({"artifacts": ["implementation_plan"], "message": "Plan ready"})),
            ]),
            json!([
                call("t2", "task_boundary", json!({"task_name": "Edit notes", "summary": "Applying the plan", "phase": "executing"})),
                call("c1", "read_file", json!({"file_path": "notes.txt"})),
                call("c2", "replace", json!({"file_path": "notes.txt", "instruction": "greeting", "old_string": "hello world", "new_string": "hello planned world"})),
            ]),
            json!([
                call("w2", "write_artifact", json!({"name": "walkthrough", "content": WALKTHROUGH, "summary": "Done"})),
                call("d1", "task_complete", json!({"result": "Updated the greeting as planned."})),
            ]),
        ];
        let (endpoint, m) = mock(script).await;
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("notes.txt"), "hello world\n").unwrap();
        let settings = Settings {
            workspace_history: vec![ws.path().to_path_buf()],
            agent_mode: AgentMode::Planning,
            artifact_review: ArtifactReviewPolicy::RequestReview,
            auto_approve_read_tools: true,
            auto_approve_edits: true,
            ..Settings::default()
        };
        let (cmd, mut rx, shutdown) = start(data.path(), &settings, endpoint);
        cmd.send(AppCommand::NewChat).unwrap();
        cmd.send(AppCommand::SendPrompt("Change the greeting in notes.txt".into())).unwrap();

        let mut reviews = 0;
        let (id, events) = run_turn(&mut rx, |names| {
            reviews += 1;
            assert_eq!(names, ["implementation_plan"]);
            ReviewOutcome::ApprovedWithComments(vec![ArtifactComment {
                artifact: "implementation_plan".into(),
                anchor: None,
                text: "Keep the trailing newline".into(),
            }])
        })
        .await;
        assert_eq!(reviews, 1);

        // Events: task groups, both artifacts, every tool succeeded, completion.
        let tasks: Vec<String> =
            events.iter().filter_map(|e| match e { AgentEvent::TaskUpdate(t) => Some(t.task_name.clone()), _ => None }).collect();
        assert!(tasks.contains(&"Plan the change".to_string()) && tasks.contains(&"Edit notes".to_string()), "{tasks:?}");
        let arts: Vec<(String, u32)> = events
            .iter()
            .filter_map(|e| match e { AgentEvent::ArtifactChanged { name, version, .. } => Some((name.clone(), *version)), _ => None })
            .collect();
        assert_eq!(arts, vec![("implementation_plan".into(), 1), ("walkthrough".into(), 1)]);
        for tool in ["task_boundary", "read_file", "replace", "write_artifact", "request_review", "task_complete"] {
            assert!(
                events.iter().any(|e| matches!(e, AgentEvent::ToolCallResponse { name, ok: true, .. } if name == tool)),
                "{tool} should succeed"
            );
        }
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::ToolCallResponse { ok: false, .. })), "no tool may fail");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TaskComplete { result } if result.contains("as planned"))));
        let Some(AgentEvent::Finished { reason, history }) = events.last() else { panic!("no Finished") };
        assert_eq!(*reason, FinishReason::Completed);

        // Workspace edited without a prompt; artifacts on disk.
        assert_eq!(std::fs::read_to_string(ws.path().join("notes.txt")).unwrap(), "hello planned world\n");
        let store = ConversationStore::new(data.path()).artifacts(id);
        assert_eq!(store.read("implementation_plan").unwrap(), PLAN);
        assert_eq!(store.read("walkthrough").unwrap(), WALKTHROUGH);

        // History: prompt, 3 x (model calls, user responses), closing result text.
        assert_eq!(history.len(), 8);
        assert_alternates(history);
        assert_eq!(history[0].text(), "Change the greeting in notes.txt");
        assert_eq!(history.last().unwrap().text(), "Updated the greeting as planned.");
        let conv = saved(data.path(), id, 8).await;
        assert_eq!(&conv.messages, history);
        let plan = conv.artifacts.iter().find(|a| a.name == "implementation_plan").expect("plan metadata saved");
        assert_eq!(plan.review, ReviewState::Approved);
        assert!(conv.artifacts.iter().any(|a| a.name == "walkthrough"));

        // Wire: planning tools offered; the review comment goes back with the approval.
        let bodies = m.bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3);
        assert!(bodies[0]["request"]["tools"].to_string().contains("request_review"));
        let second = bodies[1]["request"]["contents"].to_string();
        assert!(second.contains("Keep the trailing newline") && second.contains("approved"), "{second}");
        shutdown.cancel();
    }

    #[tokio::test]
    async fn denylisted_command_never_auto_runs_in_turbo() {
        let script = vec![
            json!([
                call("s1", "run_shell_command", json!({"command": "Remove-Item victim.txt"})),
                call("s2", "run_shell_command", json!({"command": "echo ok; Remove-Item victim.txt"})),
                call("s3", "run_shell_command", json!({"command": "Write-Output $(Remove-Item victim.txt)"})),
            ]),
            json!([{"text": "Those were refused."}]),
        ];
        let (endpoint, m) = mock(script).await;
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("victim.txt"), "keep me").unwrap();
        let mut settings = Settings {
            workspace_history: vec![ws.path().to_path_buf()],
            agent_mode: AgentMode::Fast,
            terminal_policy: TerminalPolicy::Turbo,
            ..Settings::default()
        };
        settings.terminal_denylist.push("remove-item".into());
        let (cmd, mut rx, shutdown) = start(data.path(), &settings, endpoint);
        cmd.send(AppCommand::SendPrompt("delete victim.txt".into())).unwrap();
        // `run_turn` panics on any Confirmation: denied calls must not even ask.
        let (_, events) = run_turn(&mut rx, |_| ReviewOutcome::Cancelled).await;
        let shell: Vec<(bool, String)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCallResponse { name, ok, display, .. } if name == "run_shell_command" => Some((*ok, display.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(shell.len(), 3);
        assert!(shell.iter().all(|(ok, d)| !ok && d.contains("denylist")), "{shell:?}");
        assert!(ws.path().join("victim.txt").exists(), "the file must survive");
        assert!(matches!(events.last(), Some(AgentEvent::Finished { reason: FinishReason::Completed, .. })));
        let second = m.bodies.lock().unwrap()[1]["request"]["contents"].to_string();
        assert!(second.contains("denylist"), "the model is told why");
        shutdown.cancel();
    }
}
