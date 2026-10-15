use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::net::UnixListener;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sidevoice-cursor-identity-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }

    fn instance(&self, profile: &str, socket_name: &str) -> BridgeInstance {
        let user_data = self.0.join(profile);
        fs::create_dir_all(&user_data).unwrap();
        BridgeInstance {
            socket: self.0.join(socket_name),
            token: "fixture-token".into(),
            user_data,
            created_at: 1,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn serve_once(instance: &BridgeInstance, body: Value) -> tokio::task::JoinHandle<Value> {
    let listener = UnixListener::bind(&instance.socket).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut block = [0; 4096];
            let size = stream.read(&mut block).await.unwrap();
            assert!(size > 0 && request.len() + size < 16384);
            request.extend_from_slice(&block[..size]);
            if let Some(split) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..split]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("Content-Length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap();
                if request.len() >= split + 4 + length {
                    break;
                }
            }
        }
        let split = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        let sent = serde_json::from_slice(&request[split + 4..]).unwrap();
        let payload = body.to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        sent
    })
}

#[tokio::test]
async fn same_bridge_composer_reuses_a_namespaced_thread_after_reconnect() {
    let fixture = Fixture::new();
    let first = fixture.instance("profile-a", "first.sock");
    let response = json!({"threads":[{"id":"composer-1","source":"local","status":"running"}]});
    let reply = serve_once(&first, response.clone());
    let mut join = identity(
        &json!({"name":"cursor-vscode"}),
        &json!({"extensions":{"io.modelcontextprotocol/ui":{}}}),
    )
    .unwrap();
    refine_identity_with_instances(&mut join, "Example", std::slice::from_ref(&first)).await;
    let chat = BridgeChat {
        composer: "composer-1".into(),
        profile_id: profile_id(&first.user_data).unwrap(),
    };
    assert_eq!(join.thread, chat.thread());
    assert_eq!(join.delivery["identity_state"], "candidate");
    assert_eq!(join.delivery["candidate"], "composer-1");
    assert_eq!(reply.await.unwrap()["type"], "listThreads");
    assert_eq!(
        chat.thread(),
        format!(
            "cursor-editor-{}-composer-1",
            profile_id(&first.user_data).unwrap()
        )
    );
    assert!(!chat
        .thread()
        .contains(&fixture.0.to_string_lossy().to_string()));

    let restarted = fixture.instance("profile-a", "second.sock");
    let reply = serve_once(&restarted, response);
    assert_eq!(calling_thread(&[restarted]).await.unwrap(), chat);
    reply.await.unwrap();

    let next = fixture.instance("profile-a", "third.sock");
    let reply = serve_once(
        &next,
        json!({"threads":[{"id":"composer-2","source":"local","status":"running"}]}),
    );
    assert_ne!(
        calling_thread(&[next]).await.unwrap().thread(),
        chat.thread()
    );
    reply.await.unwrap();

    let other = fixture.instance("profile-b", "fourth.sock");
    let reply = serve_once(
        &other,
        json!({"threads":[{"id":"composer-1","source":"local","status":"running"}]}),
    );
    assert_ne!(
        calling_thread(&[other]).await.unwrap().thread(),
        chat.thread()
    );
    reply.await.unwrap();

    remember_chat(&fixture.0, &chat).unwrap();
    remember_chat(&fixture.0, &chat).unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(fixture.0.join("cursor-editor-threads.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved[format!("{}:{}", chat.profile_id, chat.composer)],
        chat.thread()
    );
}

#[tokio::test]
async fn ambiguous_or_unsafe_running_bridge_chat_keeps_identity_pending() {
    let fixture = Fixture::new();
    let instance = fixture.instance("profile-a", "ambiguous.sock");
    let reply = serve_once(
        &instance,
        json!({"threads":[
            {"id":"composer-1","source":"local","status":"running"},
            {"id":"composer-2","source":"local","status":"running"}
        ]}),
    );
    let mut identity = identity(
        &json!({"name":"cursor-vscode"}),
        &json!({"extensions":{"io.modelcontextprotocol/ui":{}}}),
    )
    .unwrap();
    refine_identity_with_instances(&mut identity, "", &[instance]).await;
    assert!(identity.thread.starts_with("cursor-editor-pending-"));
    reply.await.unwrap();
    let unsafe_instance = fixture.instance("profile-a", "unsafe.sock");
    let reply = serve_once(
        &unsafe_instance,
        json!({"threads":[{"id":"../secret","source":"local","status":"running"}]}),
    );
    assert!(calling_thread(&[unsafe_instance]).await.is_none());
    reply.await.unwrap();
}

#[tokio::test]
async fn editor_delivery_is_confined_to_the_verified_bridge_profile() {
    let fixture = Fixture::new();
    let wrong = fixture.instance("profile-b", "wrong.sock");
    let right = fixture.instance("profile-a", "right.sock");
    let wrong_listener = UnixListener::bind(&wrong.socket).unwrap();
    let response = serve_once(&right, json!({"outcome":"submitted"}));
    let chat = BridgeChat {
        composer: "composer-1".into(),
        profile_id: profile_id(&right.user_data).unwrap(),
    };
    let delivery = json!({"kind":"cursor-bridge","identity_state":"verified","thread":chat.thread(),"composer":chat.composer,"profile_id":chat.profile_id});
    let event =
        json!({"channel":"voice","session_id":"s","revision":1,"message_id":"m","text":"hello"});
    let answer = deliver_editor_with_instances(&delivery, &event, &[wrong, right])
        .await
        .unwrap();
    assert_eq!(answer["status"], "accepted");
    let sent = response.await.unwrap();
    assert_eq!(sent["type"], "sendMessage");
    assert_eq!(sent["threadId"], "composer-1");
    assert!(
        tokio::time::timeout(Duration::from_millis(40), wrong_listener.accept())
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn late_marker_in_sqlite_corrects_an_earlier_running_candidate() {
    if std::process::Command::new("sqlite3")
        .arg("-version")
        .output()
        .is_err()
    {
        return;
    }
    let fixture = Fixture::new();
    let instance = fixture.instance("profile-a", "unused.sock");
    let storage = instance.user_data.join("User/globalStorage");
    fs::create_dir_all(&storage).unwrap();
    let db = storage.join("state.vscdb");
    let marker = "a".repeat(64);
    let sql = format!("CREATE TABLE cursorDiskKV(key TEXT, value BLOB); INSERT INTO cursorDiskKV VALUES ('bubbleId:actual-chat:1','{marker}');");
    let result = Command::new("sqlite3").arg(&db).arg(sql).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        query_bridge_chat(std::slice::from_ref(&instance), &marker, Some("wrong-chat"))
            .await
            .is_none()
    );
    let delivery = json!({"marker":marker,"candidate":"wrong-chat"});
    let corrected =
        composer_holding_identity_with_instances(&delivery, std::slice::from_ref(&instance))
            .await
            .unwrap();
    assert_eq!(corrected.composer, "actual-chat");
    assert_eq!(
        corrected.thread(),
        format!(
            "cursor-editor-{}-actual-chat",
            profile_id(&instance.user_data).unwrap()
        )
    );
}
