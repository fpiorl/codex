use anyhow::Result;
use codex_config::config_toml::PrivacyRuleToml;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn rules() -> Vec<PrivacyRuleToml> {
    vec![
        PrivacyRuleToml {
            real: "google.com".to_string(),
            placeholder: Some("search-co.example".to_string()),
        },
        PrivacyRuleToml {
            real: "Google".to_string(),
            placeholder: Some("SearchCo".to_string()),
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn privacy_filter_round_trips_through_a_full_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "privacy-call";
    // The mock model only ever speaks in placeholders: it runs a command that
    // mentions the placeholder domain and then answers with the placeholder name.
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_exec_command_call(call_id, "echo host=search-co.example owner=SearchCo"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "Done: SEARCH-CO.EXAMPLE belongs to SearchCo."),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.privacy_rules = rules();
        })
        .build(&server)
        .await?;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Check google.com for Google, please".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(SandboxPolicy::DangerFullAccess),
                ..Default::default()
            }),
        )
        .await?;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), test.codex.next_event())
            .await
            .expect("timed out waiting for turn events")?;
        let done = matches!(event.msg, EventMsg::TurnComplete(_));
        events.push(event.msg);
        if done {
            break;
        }
    }

    // 1. Nothing sent to the model contains the real values.
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let body = String::from_utf8(request.body_bytes())?;
        assert!(
            !body.to_ascii_lowercase().contains("google"),
            "real value leaked to the model: {body}"
        );
    }
    assert!(requests[0].body_contains_text("Check search-co.example for SearchCo, please"));

    // 2. The shell ran the *real* command (placeholders restored on the way in)...
    let exec_end = events
        .iter()
        .find_map(|event| match event {
            EventMsg::ExecCommandEnd(end) => Some(end.clone()),
            _ => None,
        })
        .expect("exec command end event");
    assert!(
        exec_end
            .aggregated_output
            .contains("host=google.com owner=Google"),
        "shell output: {}",
        exec_end.aggregated_output
    );

    // 3. ...and its output went back to the model with placeholders again.
    let tool_output = requests[1]
        .function_call_output_text(call_id)
        .expect("function call output");
    assert!(
        tool_output.contains("host=search-co.example owner=SearchCo"),
        "tool output sent to model: {tool_output}"
    );

    // 4. The user sees the real names in the final message, case shape preserved.
    let message = events
        .iter()
        .rev()
        .find_map(|event| match event {
            EventMsg::AgentMessage(message) => Some(message.message.clone()),
            _ => None,
        })
        .expect("agent message");
    assert_eq!(message, "Done: GOOGLE.COM belongs to Google.");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn privacy_filter_covers_file_reads_and_writes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(test_codex().with_config(|config| {
        config.privacy_rules = rules();
    }))
    .await?;
    let read_call_id = "read-call";
    let write_call_id = "write-call";
    // Turn 1: model reads a file that contains the real values.
    // Turn 2: model writes a new file using only placeholders.
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_exec_command_call(read_call_id, "cat secrets.txt"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_apply_patch_custom_tool_call(
                    write_call_id,
                    "*** Begin Patch\n*** Add File: notes.md\n+Contact SearchCo at https://mail.search-co.example\n*** End Patch",
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "Wrote notes.md for SearchCo."),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let test = harness.test();
    std::fs::write(
        test.cwd_path().join("secrets.txt"),
        "api endpoint: https://api.google.com/v1 (owner: Google)\n",
    )?;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "read secrets.txt then write notes.md".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(SandboxPolicy::DangerFullAccess),
                ..Default::default()
            }),
        )
        .await?;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), test.codex.next_event())
            .await
            .expect("timed out waiting for turn events")?;
        if matches!(event.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let body = String::from_utf8(request.body_bytes())?;
        assert!(
            !body.to_ascii_lowercase().contains("google"),
            "real value leaked to the model: {body}"
        );
    }
    // The file content reached the model with placeholders.
    let read_output = requests[1]
        .function_call_output_text(read_call_id)
        .expect("read output");
    assert!(
        read_output.contains("https://api.search-co.example/v1 (owner: SearchCo)"),
        "file content sent to model: {read_output}"
    );
    // The file the model wrote landed on disk with the real values.
    let written = std::fs::read_to_string(test.cwd_path().join("notes.md"))?;
    assert_eq!(written, "Contact Google at https://mail.google.com\n");
    Ok(())
}
