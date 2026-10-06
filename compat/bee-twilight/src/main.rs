//! Drives twilight 0.16 with bee-harness's exact features and intents against a running
//! Fauxcord, and reports which parts of the Discord surface that bot relies on work.
//!
//!   FAUXCORD_PORT=3999 cargo run   # exits 1 when any step fails
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};
use twilight_gateway::{CloseFrame, ConfigBuilder, Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client;
use twilight_model::application::command::{Command as AppCommand, CommandType};
use twilight_model::channel::message::AllowedMentions;
use twilight_model::channel::ChannelType;
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType};
use twilight_model::id::Id;

const TOKEN: &str = "probe-token";
const BOT: u64 = 111111111111111111;
const GUILD: u64 = 222222222222222222;
const CHANNEL: u64 = 333333333333333333;
const ALICE: u64 = 555555555555555555;

fn port() -> String {
    std::env::var("FAUXCORD_PORT").unwrap_or_else(|_| "3999".into())
}

fn control(method: &str, path: &str, body: Option<Value>) -> Value {
    let mut cmd = Command::new("curl");
    let base = format!("http://127.0.0.1:{}", port());
    cmd.args(["-s", "-X", method, &format!("{base}{path}"), "-H", "content-type: application/json"]);
    if let Some(body) = body {
        cmd.args(["-d", &body.to_string()]);
    }
    let out = cmd.output().expect("curl");
    serde_json::from_slice(&out.stdout).unwrap_or(Value::String(String::from_utf8_lossy(&out.stdout).into()))
}

static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn report(step: &str, result: Result<String, String>) {
    match result {
        Ok(note) => println!("PASS  {step:38} {note}"),
        Err(note) => {
            FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            println!("FAIL  {step:38} {note}")
        }
    }
}

async fn next_matching<T>(shard: &mut Shard, secs: u64, mut pick: impl FnMut(&Event) -> Option<T>) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match shard.next_event(EventTypeFlags::all()).await {
                Some(Ok(event)) => {
                    if let Some(found) = pick(&event) {
                        return Ok(found);
                    }
                }
                Some(Err(error)) => return Err(format!("gateway error: {error:?}")),
                None => return Err("stream ended".into()),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("timeout".into()))
}

#[tokio::main]
async fn main() {
    control("DELETE", &format!("/_test/setup/Bot%20{TOKEN}"), None);
    let setup = control(
        "POST",
        "/_test/setup",
        Some(json!({
            "token": format!("Bot {TOKEN}"),
            "user": {"id": BOT.to_string(), "username": "Bee"},
            "guilds": [{"id": GUILD.to_string(), "name": "Probe Guild",
                        "channels": [{"id": CHANNEL.to_string(), "name": "general", "type": 0}]}]
        })),
    );
    println!("setup: {}", setup.to_string().chars().take(160).collect::<String>());
    let alice = control("POST", "/_test/users", Some(json!({"id": ALICE.to_string(), "username": "alice"})));
    println!("alice: {}", alice.to_string().chars().take(120).collect::<String>());
    control("POST", &format!("/_test/guilds/{GUILD}/members/{ALICE}"), Some(json!({})));

    let http = Client::builder().token(TOKEN.into()).proxy(format!("127.0.0.1:{}", port()), true).build();
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::DIRECT_MESSAGES | Intents::MESSAGE_CONTENT;
    let config = ConfigBuilder::new(TOKEN.into(), intents).proxy_url(format!("ws://127.0.0.1:{}", port())).build();
    let mut shard = Shard::with_config(ShardId::ONE, config);

    // 1. READY + GUILD_CREATE
    let ready = next_matching(&mut shard, 15, |e| match e {
        Event::Ready(r) => Some(format!("user={} guilds={}", r.user.id, r.guilds.len())),
        _ => None,
    })
    .await;
    report("gateway READY (raw token, zlib-stock)", ready.clone());
    if ready.is_err() {
        std::process::exit(1);
    }
    report(
        "gateway GUILD_CREATE",
        next_matching(&mut shard, 5, |e| match e {
            Event::GuildCreate(g) => Some(format!("{:?}", g.id())),
            _ => None,
        })
        .await,
    );

    // 2. a human mentions the bot in a guild channel
    control(
        "POST",
        &format!("/_test/channels/{CHANNEL}/messages"),
        Some(json!({"content": format!("<@{BOT}> xin chào"), "author": {"id": ALICE.to_string()}})),
    );
    let mention = next_matching(&mut shard, 5, |e| match e {
        Event::MessageCreate(m) if m.author.id.get() == ALICE => Some((
            m.id,
            format!(
                "content={:?} bot_mentioned={} guild={:?} member={}",
                m.content,
                m.mentions.iter().any(|u| u.id.get() == BOT),
                m.guild_id,
                m.member.is_some()
            ),
        )),
        _ => None,
    })
    .await;
    report("human mention -> MESSAGE_CREATE", mention.clone().map(|(_, n)| n));

    // 3. reply with message_reference + allowed_mentions
    if let Ok((message_id, _)) = mention {
        let reply = http
            .create_message(Id::new(CHANNEL))
            .content("Chào alice!")
            .reply(message_id)
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await;
        report(
            "REST reply (message_reference)",
            match reply {
                Ok(r) => r.model().await.map(|m| format!("id={} ref={:?}", m.id, m.reference.and_then(|r| r.message_id))).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            },
        );
    }
    report("REST typing trigger", http.create_typing_trigger(Id::new(CHANNEL)).await.map(|_| String::new()).map_err(|e| e.to_string()));

    // 4. Bee opens a public thread under the channel, the human talks in it
    let thread = match http.create_thread(Id::new(CHANNEL), "Bee · alice", ChannelType::PublicThread).await {
        Ok(r) => r.model().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    report("REST create_thread (standalone)", thread.as_ref().map(|t| format!("id={} parent={:?}", t.id, t.parent_id)).map_err(Clone::clone));
    if let Ok(thread) = thread {
        report(
            "gateway THREAD_CREATE",
            next_matching(&mut shard, 5, |e| match e {
                Event::ThreadCreate(t) => Some(format!("id={}", t.id)),
                _ => None,
            })
            .await,
        );
        control(
            "POST",
            &format!("/_test/channels/{}/messages", thread.id),
            Some(json!({"content": "trong thread", "author": {"id": ALICE.to_string()}})),
        );
        report(
            "human message in thread",
            next_matching(&mut shard, 5, |e| match e {
                Event::MessageCreate(m) if m.author.id.get() == ALICE => Some(format!("channel={} content={:?}", m.channel_id, m.content)),
                _ => None,
            })
            .await,
        );
    }

    // 5. DM
    let dm = match http.create_private_channel(Id::new(ALICE)).await {
        Ok(r) => r.model().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    report("REST create_private_channel", dm.as_ref().map(|c| format!("id={} kind={:?}", c.id, c.kind)).map_err(Clone::clone));
    if let Ok(dm) = dm {
        let injected = control(
            "POST",
            &format!("/_test/channels/{}/messages", dm.id),
            Some(json!({"content": "nhắn riêng", "author": {"id": ALICE.to_string()}})),
        );
        if injected.get("id").is_none() {
            report("human DM inject", Err(injected.to_string()));
        } else {
            report(
                "human DM -> MESSAGE_CREATE",
                next_matching(&mut shard, 5, |e| match e {
                    Event::MessageCreate(m) if m.author.id.get() == ALICE => Some(format!("guild={:?} content={:?}", m.guild_id, m.content)),
                    _ => None,
                })
                .await,
            );
            report(
                "REST send into DM",
                http.create_message(dm.id).content("trả lời DM").await.map(|_| String::new()).map_err(|e| e.to_string()),
            );
        }
    }

    // 6. attachment from a human, downloaded by URL
    control(
        "POST",
        &format!("/_test/channels/{CHANNEL}/messages"),
        Some(json!({"content": format!("<@{BOT}> xem file"), "author": {"id": ALICE.to_string()},
                    "attachments": [{"filename": "note.txt", "content_type": "text/plain", "data": "eGluIGNow6Bv"}]})),
    );
    let attachment = next_matching(&mut shard, 5, |e| match e {
        Event::MessageCreate(m) if m.author.id.get() == ALICE && !m.attachments.is_empty() => Some(m.attachments[0].url.clone()),
        _ => None,
    })
    .await;
    report(
        "attachment url download",
        attachment.map(|url| {
            let body = Command::new("curl").args(["-s", &url]).output().unwrap().stdout;
            format!("{url} -> {:?}", String::from_utf8_lossy(&body))
        }),
    );

    // 7. application, slash commands, interaction + response
    let app = match http.current_user_application().await {
        Ok(r) => r.model().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    report("REST current_user_application", app.as_ref().map(|a| format!("id={}", a.id)).map_err(Clone::clone));
    if let Ok(app) = app {
        let interactions = http.interaction(app.id);
        #[allow(deprecated)]
        let command = AppCommand {
            application_id: None,
            contexts: None,
            default_member_permissions: None,
            dm_permission: None,
            description: "Start a new conversation".into(),
            description_localizations: None,
            guild_id: None,
            id: None,
            integration_types: None,
            kind: CommandType::ChatInput,
            name: "new".into(),
            name_localizations: None,
            nsfw: None,
            options: vec![],
            version: Id::new(1),
        };
        report(
            "REST set_global_commands",
            interactions.set_global_commands(&[command]).await.map(|_| String::new()).map_err(|e| e.to_string()),
        );
        let sim = control(
            "POST",
            "/_test/interactions",
            Some(json!({"application_id": app.id.to_string(), "command_name": "new", "guild_id": GUILD.to_string(),
                        "channel_id": CHANNEL.to_string(), "user_id": ALICE.to_string()})),
        );
        let got = next_matching(&mut shard, 5, |e| match e {
            Event::InteractionCreate(i) => Some((i.id, i.token.clone())),
            _ => None,
        })
        .await;
        report("gateway INTERACTION_CREATE", got.clone().map(|(id, _)| format!("id={id}")).map_err(|e| format!("{e} sim={sim}")));
        if let Ok((id, token)) = got {
            let response = InteractionResponse {
                kind: InteractionResponseType::ChannelMessageWithSource,
                data: Some(InteractionResponseData { content: Some("Đã bắt đầu cuộc trò chuyện mới".into()), ..Default::default() }),
            };
            report(
                "REST interaction create_response",
                interactions.create_response(id, &token, &response).await.map(|_| String::new()).map_err(|e| e.to_string()),
            );
        }
    }

    // 7b. a button press (owner desk / access prompts)
    let sim = control(
        "POST",
        "/_test/interactions",
        Some(json!({"application_id": BOT.to_string(), "type": 3, "custom_id": "bee:access:approve:42",
                    "guild_id": GUILD.to_string(), "channel_id": CHANNEL.to_string(), "user_id": ALICE.to_string()})),
    );
    report(
        "gateway button press (component)",
        next_matching(&mut shard, 5, |e| match e {
            Event::InteractionCreate(i) => match &i.data {
                Some(twilight_model::application::interaction::InteractionData::MessageComponent(d)) => {
                    Some(format!("custom_id={} user={:?}", d.custom_id, i.author_id()))
                }
                _ => None,
            },
            _ => None,
        })
        .await
        .map_err(|e| format!("{e} sim={sim}")),
    );

    // 8. reads Bee does at startup / setup
    report("REST current_user_guilds", match http.current_user_guilds().await { Ok(r) => r.model().await.map(|g| format!("n={}", g.len())).map_err(|e| e.to_string()), Err(e) => Err(e.to_string()) });
    report("REST guild_channels", match http.guild_channels(Id::new(GUILD)).await { Ok(r) => r.model().await.map(|c| format!("n={}", c.len())).map_err(|e| e.to_string()), Err(e) => Err(e.to_string()) });
    report("REST channel", match http.channel(Id::new(CHANNEL)).await { Ok(r) => r.model().await.map(|c| format!("{:?}", c.name)).map_err(|e| e.to_string()), Err(e) => Err(e.to_string()) });
    report("REST channel_messages", match http.channel_messages(Id::new(CHANNEL)).await { Ok(r) => r.models().await.map(|m| format!("n={}", m.len())).map_err(|e| e.to_string()), Err(e) => Err(e.to_string()) });

    // 9. reconnect with RESUME, then a message sent while away should arrive
    shard.close(CloseFrame::RESUME);
    let _ = next_matching(&mut shard, 5, |e| matches!(e, Event::GatewayClose(_)).then_some(())).await;
    control(
        "POST",
        &format!("/_test/channels/{CHANNEL}/messages"),
        Some(json!({"content": format!("<@{BOT}> lúc mất kết nối"), "author": {"id": ALICE.to_string()}})),
    );
    report(
        "gateway RESUME replays missed message",
        next_matching(&mut shard, 15, |e| match e {
            Event::Ready(_) => Some(Err("READY: new session instead of resume".to_string())),
            Event::MessageCreate(m) if m.author.id.get() == ALICE => Some(Ok(format!("content={:?}", m.content))),
            _ => None,
        })
        .await
        .and_then(|r| r),
    );
    report(
        "gateway RESUMED after replay",
        next_matching(&mut shard, 5, |e| matches!(e, Event::Resumed).then(String::new)).await,
    );

    control("DELETE", &format!("/_test/setup/Bot%20{TOKEN}"), None);
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(1);
    }
}
