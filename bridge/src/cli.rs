//! Admin subcommands: link an account, list portal rooms, map chats.
//! (Phase 4 will expose linking inside Bens Chat; until then this is how
//! a user's WhatsApp / Google Messages gets connected.)

use std::io::{BufRead, Write};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::State;
use crate::config::network_label;
use crate::db::Chat;

pub const USAGE: &str = "\
usage: bc-bridge [command]

  (no command) | run                        run the bridge service
  link <whatsapp|gmessages> <user> [flow]   link a Bens Chat user's account (prints QR / prompts)
  rooms <user>                              list portal rooms the user's puppet is in
  create-chat <room_id> <user> <network>    create a Bens Chat chat for a portal room and map it
  map <chat_id> <room_id> <user> <network>  map an existing Bens Chat chat to a portal room
  unmap <chat_id>                           remove a mapping
  chats                                     list mappings

Restart the bridge after create-chat/map so its Bens Chat socket joins the
chat (see PLAN.md, blocker: sockets only join chats at connect time).";

pub type Error = crate::Error;

fn check_network(n: &str) -> Result<(), Error> {
    match n {
        "whatsapp" | "gmessages" => Ok(()),
        _ => Err(format!("unknown network {n:?} (whatsapp | gmessages)").into()),
    }
}

/// Make sure the user's puppet exists and is recorded.
async fn ensure_puppet(st: &State, user: &str) -> Result<String, Error> {
    st.mx
        .ensure_registered(&st.mx.puppet_localpart(user))
        .await?;
    let mxid = st.mx.puppet_id(user);
    st.db.upsert_user(user, &mxid).await?;
    Ok(mxid)
}

pub async fn chats(st: &State) -> Result<(), Error> {
    let chats = st.db.chats().await?;
    if chats.is_empty() {
        println!("no chats mapped");
    }
    for c in chats {
        println!(
            "{}  {}  {:9}  owner={}",
            c.chat_id, c.room_id, c.network, c.owner
        );
    }
    Ok(())
}

pub async fn rooms(st: &State, user: &str) -> Result<(), Error> {
    let puppet = ensure_puppet(st, user).await?;
    let rooms = st.mx.joined_rooms(&puppet).await?;
    if rooms.is_empty() {
        println!(
            "{puppet} is not in any rooms (link an account first, and keep `bc-bridge run` going so invites are accepted)"
        );
    }
    for r in rooms {
        let name = st.mx.room_name(&r, &puppet).await.unwrap_or_default();
        let mapped = match st.db.chat_by_room(&r).await? {
            Some(c) => format!("  -> chat {}", c.chat_id),
            None => String::new(),
        };
        println!("{r}  {name}{mapped}");
    }
    Ok(())
}

pub async fn map(
    st: &State,
    chat_id: &str,
    room_id: &str,
    user: &str,
    network: &str,
) -> Result<(), Error> {
    check_network(network)?;
    let chat_id = Uuid::parse_str(chat_id)?;
    ensure_puppet(st, user).await?;
    let chat = Chat {
        chat_id,
        room_id: room_id.to_owned(),
        network: network.to_owned(),
        owner: user.to_owned(),
    };
    st.db.insert_chat(&chat).await?;
    // seed the message→chat cache (also checks the bridge account is a participant)
    match st.bc.chat_tree(chat_id).await {
        Ok(msgs) => {
            for m in msgs {
                st.db.record_message(m.message_id, chat_id, None).await?;
            }
        }
        Err(e) => eprintln!(
            "warning: bridge account {} can't read chat {chat_id}: {e}\n\
             add it as a participant or messages won't bridge",
            st.bc.user()
        ),
    }
    println!("mapped chat {chat_id} <-> {room_id} ({network}, owner {user})");
    Ok(())
}

pub async fn create_chat(
    st: &State,
    room_id: &str,
    user: &str,
    network: &str,
) -> Result<(), Error> {
    check_network(network)?;
    let puppet = ensure_puppet(st, user).await?;
    let name = st
        .mx
        .room_name(room_id, &puppet)
        .await
        .unwrap_or_else(|| "Chat".to_owned());
    let title = format!("{name} ({})", network_label(network));
    let chat_id = st.bc.send(None, json!({"title": title})).await?;
    if user != st.bc.user() {
        st.bc.add_participant(chat_id, user).await?;
    }
    map(st, &chat_id.to_string(), room_id, user, network).await?;
    println!("created Bens Chat chat {title:?}");
    Ok(())
}

pub async fn unmap(st: &State, chat_id: &str) -> Result<(), Error> {
    if st.db.delete_chat(Uuid::parse_str(chat_id)?).await? {
        println!("unmapped {chat_id}");
    } else {
        println!("no mapping for {chat_id}");
    }
    Ok(())
}

// ------------------------------------------------------------------ link

fn prompt(label: &str) -> Result<String, Error> {
    print!("{label}: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn print_qr(data: &str) {
    match qrcode::QrCode::new(data.as_bytes()) {
        Ok(code) => println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>()
                .dark_color(qrcode::render::unicode::Dense1x2::Light)
                .light_color(qrcode::render::unicode::Dense1x2::Dark)
                .quiet_zone(true)
                .build()
        ),
        Err(e) => println!("(couldn't render QR: {e}) data: {data}"),
    }
}

pub async fn link(st: &State, network: &str, user: &str, flow: Option<&str>) -> Result<(), Error> {
    check_network(network)?;
    let base = st
        .cfg
        .provision_url(network)
        .ok_or("no provisioning url")?
        .trim_end_matches('/')
        .to_owned();
    if st.cfg.provision_secret.is_empty() {
        return Err("PROVISION_SECRET is not set (run scripts/setup.sh)".into());
    }
    let puppet = ensure_puppet(st, user).await?;
    // no timeout: display_and_wait steps block until the phone confirms
    let http = reqwest::Client::new();
    let call = |method: reqwest::Method, path: String, body: Option<Value>| {
        let mut req = http
            .request(method, format!("{base}/_matrix/provision/v3{path}"))
            .bearer_auth(&st.cfg.provision_secret)
            .query(&[("user_id", puppet.as_str())]);
        if let Some(b) = body {
            req = req.json(&b);
        }
        async move {
            let res = req.send().await?;
            let status = res.status();
            let v: Value = res.json().await.unwrap_or(Value::Null);
            if status.is_success() {
                Ok::<Value, Error>(v)
            } else {
                Err(format!("{status}: {}", v["error"].as_str().unwrap_or("?")).into())
            }
        }
    };

    let flows = call(reqwest::Method::GET, "/login/flows".into(), None).await?;
    let flow_ids: Vec<&str> = flows["flows"]
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["id"].as_str()).collect())
        .unwrap_or_default();
    let flow = match flow {
        Some(f) if flow_ids.contains(&f) => f.to_owned(),
        Some(f) => return Err(format!("flow {f:?} not offered; available: {flow_ids:?}").into()),
        None => flow_ids
            .first()
            .ok_or("bridge offers no login flows")?
            .to_string(),
    };
    println!(
        "linking {} for {user} ({puppet}) using flow {flow:?}",
        network_label(network)
    );

    let mut step = call(reqwest::Method::POST, format!("/login/start/{flow}"), None).await?;
    loop {
        if let Some(i) = step["instructions"].as_str() {
            println!("\n{i}");
        }
        let login_id = step["login_id"].as_str().unwrap_or_default().to_owned();
        let step_id = step["step_id"].as_str().unwrap_or_default().to_owned();
        let kind = step["type"].as_str().unwrap_or_default().to_owned();
        let body = match kind.as_str() {
            "complete" => {
                let c = &step["complete"];
                println!(
                    "\nlinked! login id: {}",
                    c["user_login_id"].as_str().unwrap_or("?")
                );
                println!(
                    "portal rooms will be created as chats arrive; see `bc-bridge rooms {user}`"
                );
                return Ok(());
            }
            "display_and_wait" => {
                let d = &step["display_and_wait"];
                let data = d["data"].as_str().unwrap_or_default();
                match d["type"].as_str() {
                    Some("qr") => print_qr(data),
                    _ => println!("\n    {data}\n"),
                }
                if let Some(img) = d["image_url"].as_str() {
                    println!("image: {img}");
                }
                println!("waiting…");
                json!({})
            }
            "user_input" => {
                let mut out = serde_json::Map::new();
                for f in step["user_input"]["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    let label = format!(
                        "{} ({})",
                        f["name"].as_str().unwrap_or("value"),
                        f["description"].as_str().unwrap_or("")
                    );
                    out.insert(
                        f["id"].as_str().unwrap_or_default().to_owned(),
                        prompt(&label)?.into(),
                    );
                }
                Value::Object(out)
            }
            "cookies" => {
                let c = &step["cookies"];
                println!(
                    "\nLog in at {} in a browser, then copy these cookie values from devtools \
                     (Application → Cookies). Leave optional ones empty.",
                    c["url"].as_str().unwrap_or("?")
                );
                let mut out = serde_json::Map::new();
                for f in c["fields"].as_array().into_iter().flatten() {
                    let id = f["id"].as_str().unwrap_or_default();
                    let req = f["required"].as_bool().unwrap_or(false);
                    let v = prompt(&format!("{id}{}", if req { "" } else { " (optional)" }))?;
                    if !v.is_empty() {
                        out.insert(id.to_owned(), v.into());
                    }
                }
                Value::Object(out)
            }
            other => return Err(format!("unsupported login step type {other:?}: {step}").into()),
        };
        step = call(
            reqwest::Method::POST,
            format!("/login/step/{login_id}/{step_id}/{kind}"),
            Some(body),
        )
        .await?;
    }
}
