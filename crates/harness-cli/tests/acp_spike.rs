//! T0.5 throwaway spike, see ADR 0003. Delete once T5.1 has real ACP tests.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as v1;
use agent_client_protocol::{Agent, Client, Error, on_receive_notification, on_receive_request};
use tokio::sync::Mutex;

/// Echoes the prompt only after the client has allowed one tool call.
fn spike_agent() -> impl agent_client_protocol::ConnectTo<Client> {
    Agent
        .v1()
        .on_receive_request(
            async |req: v1::InitializeRequest, responder, _cx| {
                responder.respond(v1::InitializeResponse::new(req.protocol_version))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |_req: v1::NewSessionRequest, responder, _cx| {
                responder.respond(v1::NewSessionResponse::new("spike-session"))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |req: v1::PromptRequest, responder, cx| {
                // Awaiting the client here would hold the dispatch loop and deadlock, so spawn.
                cx.clone().spawn(async move {
                    let session = req.session_id.clone();
                    let call = v1::ToolCallUpdate::new(
                        "call-1",
                        v1::ToolCallUpdateFields::new()
                            .title("Echo")
                            .kind(v1::ToolKind::Edit)
                            .content(vec![
                                v1::Diff::new("/spike.txt", "new").old_text("old").into(),
                            ]),
                    );
                    let options = vec![
                        v1::PermissionOption::new(
                            "allow",
                            "Allow",
                            v1::PermissionOptionKind::AllowOnce,
                        ),
                        v1::PermissionOption::new(
                            "reject",
                            "Reject",
                            v1::PermissionOptionKind::RejectOnce,
                        ),
                    ];
                    let answer = cx
                        .send_request(v1::RequestPermissionRequest::new(
                            session.clone(),
                            call,
                            options,
                        ))
                        .block_task()
                        .await?;
                    let v1::RequestPermissionOutcome::Selected(selected) = answer.outcome else {
                        return responder
                            .respond(v1::PromptResponse::new(v1::StopReason::Cancelled));
                    };
                    assert_eq!(selected.option_id.to_string(), "allow");
                    for block in req.prompt {
                        cx.send_notification(v1::SessionNotification::new(
                            session.clone(),
                            v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk::new(block)),
                        ))?;
                    }
                    responder.respond(v1::PromptResponse::new(v1::StopReason::EndTurn))
                })
            },
            on_receive_request!(),
        )
}

#[tokio::test]
async fn agent_echoes_prompt_after_permission_is_granted() -> Result<(), Error> {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let (permission_log, update_log) = (log.clone(), log.clone());

    let run = Client
        .v1()
        .on_receive_request(
            async move |req: v1::RequestPermissionRequest, responder, _cx| {
                permission_log
                    .lock()
                    .await
                    .push(format!("permission:{}", req.tool_call.tool_call_id));
                responder.respond(v1::RequestPermissionResponse::new(
                    v1::RequestPermissionOutcome::Selected(v1::SelectedPermissionOutcome::new(
                        "allow",
                    )),
                ))
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |note: v1::SessionNotification, _cx| {
                if let v1::SessionUpdate::AgentMessageChunk(v1::ContentChunk {
                    content: v1::ContentBlock::Text(text),
                    ..
                }) = note.update
                {
                    update_log.lock().await.push(format!("chunk:{}", text.text));
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_with(spike_agent(), async |cx| {
            cx.send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(v1::NewSessionRequest::new("/tmp"))
                .block_task()
                .await?;
            let reply = cx
                .send_request(v1::PromptRequest::new(
                    session.session_id,
                    vec!["hello".into()],
                ))
                .block_task()
                .await?;
            assert_eq!(reply.stop_reason, v1::StopReason::EndTurn);
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .map_err(|_| Error::internal_error().data("spike deadlocked"))??;

    // The chunk arrives before the prompt response: the ordering SPEC §11 cancellation relies on.
    assert_eq!(*log.lock().await, ["permission:call-1", "chunk:hello"]);
    Ok(())
}

/// Pins the wire facts recorded in SPEC §11, so a crate bump that changes them fails here.
#[test]
fn schema_wire_format_matches_spec() -> Result<(), serde_json::Error> {
    use serde_json::{from_value, json, to_value};

    assert_eq!(
        to_value([
            v1::PermissionOptionKind::AllowOnce,
            v1::PermissionOptionKind::AllowAlways,
            v1::PermissionOptionKind::RejectOnce,
            v1::PermissionOptionKind::RejectAlways,
        ])?,
        json!(["allow_once", "allow_always", "reject_once", "reject_always"])
    );
    assert_eq!(
        to_value([
            v1::ToolKind::Read,
            v1::ToolKind::Edit,
            v1::ToolKind::Delete,
            v1::ToolKind::Move,
            v1::ToolKind::Search,
            v1::ToolKind::Execute,
            v1::ToolKind::Think,
            v1::ToolKind::Fetch,
            v1::ToolKind::SwitchMode,
            v1::ToolKind::Other,
        ])?,
        json!([
            "read",
            "edit",
            "delete",
            "move",
            "search",
            "execute",
            "think",
            "fetch",
            "switch_mode",
            "other"
        ])
    );
    let diff: v1::ToolCallContent = v1::Diff::new("/a", "new").old_text("old").into();
    assert_eq!(
        to_value(diff)?,
        json!({"type": "diff", "path": "/a", "oldText": "old", "newText": "new"})
    );
    let session: v1::NewSessionRequest = from_value(json!({
        "cwd": "/w",
        "mcpServers": [{"name": "m", "command": "/bin/m", "args": [], "env": []}],
    }))?;
    assert!(matches!(session.mcp_servers[..], [v1::McpServer::Stdio(_)]));
    assert!(from_value::<v1::NewSessionRequest>(json!({"cwd": "/w"})).is_err());
    Ok(())
}
