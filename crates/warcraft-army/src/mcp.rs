//! A minimal MCP (Model Context Protocol) server for the Army App, over HTTP JSON-RPC 2.0. It's the
//! groundwork for the agentic workshop adapter: an attendee's agent (or Claude, via `curl`) talks
//! MCP tool-calls here, and we translate them into `benilla-protocol` commands to a logged-in bot.
//! The server stays authoritative — we only relay; the WoW server's anti-cheat adjudicates.
//!
//! Transport: HTTP POST on 127.0.0.1:8711, body = a JSON-RPC request. Methods: `initialize`,
//! `tools/list`, `tools/call`. First tools: `list_bots`, `raw_move` (EXACT position, no clamp — the
//! adversarial / anti-cheat-test primitive), `move_to` (honest walk).
//!
//! Example:
//!   curl -s -X POST http://127.0.0.1:8711 -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
//!     "params":{"name":"raw_move","arguments":{"user":"ArmyBot","x":-8927,"y":-132,"z":132}}}'

use crate::manager::{ControlHandle, MovePlan, MoveState};
use serde_json::{json, Value};
use std::thread;

const ADDR: &str = "127.0.0.1:8711";

/// Spawn the MCP HTTP server on its own thread. Non-fatal if the port is taken.
pub fn serve(ctrl: ControlHandle) {
    thread::spawn(move || {
        let server = match tiny_http::Server::http(ADDR) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mcp: could not bind {ADDR}: {e}");
                return;
            }
        };
        eprintln!("mcp: Army MCP server listening on http://{ADDR}");
        for mut req in server.incoming_requests() {
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let reply = handle(&ctrl, &body);
            match reply {
                Some(json_text) => {
                    let ct =
                        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                            .unwrap();
                    let _ = req.respond(tiny_http::Response::from_string(json_text).with_header(ct));
                }
                None => {
                    // A JSON-RPC notification (e.g. notifications/initialized): 202, no body.
                    let _ = req.respond(tiny_http::Response::empty(202));
                }
            }
        }
    });
}

/// Dispatch one JSON-RPC request; `None` means "notification, send no body".
fn handle(ctrl: &ControlHandle, body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "warcraft-army", "version": "0.1.0" }
        }),
        "notifications/initialized" | "notifications/cancelled" => return None,
        "tools/list" => json!({ "tools": tool_schemas() }),
        "tools/call" => call_tool(ctrl, v.get("params").unwrap_or(&Value::Null)),
        "ping" => json!({}),
        _ => {
            return Some(
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {method}") }
                }))
                .ok()?,
            )
        }
    };
    serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).ok()
}

fn tool_schemas() -> Value {
    let xyz = json!({
        "user": { "type": "string", "description": "bot account name (see list_bots)" },
        "x": { "type": "number" }, "y": { "type": "number" }, "z": { "type": "number" }
    });
    let user_only = json!({
        "user": { "type": "string", "description": "bot account name (see list_bots)" }
    });
    json!([
        {
            "name": "list_bots",
            "description": "List every bot account with its lifecycle state (offline/connecting/char_select/in_world), roster (guid,name,level), in-world guid, and live position.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "login",
            "description": "Log the account in (async; poll list_bots until state is char_select).",
            "inputSchema": { "type": "object", "properties": user_only, "required": ["user"] }
        },
        {
            "name": "enter_world",
            "description": "Enter the world. Omit guid to use the account's first character. Async; poll list_bots until state is in_world.",
            "inputSchema": { "type": "object", "properties": {
                "user": { "type": "string" },
                "guid": { "type": "integer", "description": "character guid; omit for the first roster character" }
            }, "required": ["user"] }
        },
        {
            "name": "logout",
            "description": "Log the account out.",
            "inputSchema": { "type": "object", "properties": user_only, "required": ["user"] }
        },
        {
            "name": "raw_move",
            "description": "Move the in-world bot to an EXACT (x,y,z) with NO ground-clamp (flying/sinking allowed). The adversarial / anti-cheat-test primitive; the server adjudicates.",
            "inputSchema": { "type": "object", "properties": xyz, "required": ["user","x","y","z"] }
        },
        {
            "name": "move_to",
            "description": "Walk the in-world bot to (x,y,z) along a navmesh path (obstacle-aware). Returns a PLAN immediately: the reachable endpoint, waypoint count, distance, and move_id, plus whether it is PARTIAL (navmesh could only route part-way) or COMPLETE. The walk then runs async — poll list_bots for that bot's movement state (moving → arrived | blocked, with the matching move_id) to know when it truly arrives.",
            "inputSchema": { "type": "object", "properties": xyz, "required": ["user","x","y","z"] }
        },
        {
            "name": "create_character",
            "description": "Create a character on a logged-in account (use after login, before enter_world, when the account has no character). race/class/gender are raw ids; omit for Human Warrior male. Human=1; Warrior=1, Paladin=2, Rogue=4, Priest=5, Mage=8; gender 0=male 1=female.",
            "inputSchema": { "type": "object", "properties": {
                "user": { "type": "string" },
                "name": { "type": "string", "description": "character name (letters)" },
                "race": { "type": "integer", "description": "ChrRaces id; default 1 (Human)" },
                "class": { "type": "integer", "description": "ChrClasses id; default 1 (Warrior)" },
                "gender": { "type": "integer", "description": "0 male, 1 female; default 0" }
            }, "required": ["user","name"] }
        },
        {
            "name": "add_account",
            "description": "Register an account's credentials in the running adapter (no restart). Then login/enter/move it.",
            "inputSchema": { "type": "object", "properties": {
                "user": { "type": "string" },
                "password": { "type": "string" },
                "realmlist": { "type": "string", "description": "host[:port], default 127.0.0.1" }
            }, "required": ["user","password"] }
        }
    ])
}

fn call_tool(ctrl: &ControlHandle, params: &Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let arg_str = |k: &str| args.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let arg_f32 = |k: &str| args.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;

    let (text, is_err) = match name {
        "list_bots" => (render_bots(ctrl), false),
        "login" => {
            let user = arg_str("user");
            (format!("login: requested for '{user}' (poll list_bots)"), !ctrl.login(&user))
        }
        "enter_world" => {
            let user = arg_str("user");
            let guid = args.get("guid").and_then(|x| x.as_u64());
            let which = guid.map_or("first character".to_string(), |g| format!("guid {g}"));
            (format!("enter_world: requested for '{user}' ({which}); poll list_bots"), !ctrl.enter(&user, guid))
        }
        "logout" => {
            let user = arg_str("user");
            (format!("logout: requested for '{user}'"), !ctrl.logout(&user))
        }
        "raw_move" => {
            let user = arg_str("user");
            let pos = [arg_f32("x"), arg_f32("y"), arg_f32("z")];
            if ctrl.raw_move(&user, pos) {
                (format!("raw_move: sent to '{user}' -> ({:.2}, {:.2}, {:.2})", pos[0], pos[1], pos[2]), false)
            } else {
                (format!("raw_move: FAILED — '{user}' has no live session (login + enter_world first)"), true)
            }
        }
        "move_to" => {
            let user = arg_str("user");
            let dest = [arg_f32("x"), arg_f32("y"), arg_f32("z")];
            let plan = ctrl.move_to(&user, dest);
            (render_move_plan(&user, &plan), !plan.accepted)
        }
        "create_character" => {
            let user = arg_str("user");
            let cname = arg_str("name");
            let arg_u8 = |k: &str, dflt: u8| args.get(k).and_then(|x| x.as_u64()).map_or(dflt, |v| v as u8);
            let race = arg_u8("race", 1);
            let class = arg_u8("class", 1);
            let gender = arg_u8("gender", 0);
            (format!("create_character: requested '{cname}' on '{user}' (race {race}, class {class}, gender {gender}); then poll list_bots roster"), !ctrl.create_char(&user, &cname, race, class, gender))
        }
        "add_account" => {
            let user = arg_str("user");
            let pass = arg_str("password");
            let realm = { let r = arg_str("realmlist"); if r.is_empty() { "127.0.0.1".to_string() } else { r } };
            (format!("add_account: registered '{user}' (now call login for it)"), !ctrl.add_account(&user, &pass, &realm))
        }
        other => (format!("unknown tool '{other}'"), true),
    };
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_err })
}

/// Render an accepted/rejected MoveTo plan for the `move_to` tool.
fn render_move_plan(user: &str, p: &MovePlan) -> String {
    if !p.accepted {
        return format!(
            "move_to REJECTED for '{user}': {}",
            p.reason.as_deref().unwrap_or("unknown")
        );
    }
    let short = (p.endpoint[0] - p.requested[0]).hypot(p.endpoint[1] - p.requested[1]);
    let tag = if p.partial {
        format!("PARTIAL — {short:.0} yd short of requested")
    } else {
        "COMPLETE".to_string()
    };
    format!(
        "move_to accepted for '{user}' (move #{})\n  \
         requested:          ({:.1}, {:.1}, {:.1})\n  \
         plan:               path found · {} waypoints · ~{:.0} yd\n  \
         reachable endpoint: ({:.1}, {:.1}, {:.1})  [{}]\n  \
         poll list_bots for arrival (moving → arrived | blocked).",
        p.move_id,
        p.requested[0], p.requested[1], p.requested[2],
        p.waypoints, p.distance,
        p.endpoint[0], p.endpoint[1], p.endpoint[2],
        tag,
    )
}

/// A human-readable table of all bots for the `list_bots` tool.
fn render_bots(ctrl: &ControlHandle) -> String {
    let mut out = String::from("bots:\n");
    for (user, s) in ctrl.snapshot() {
        out.push_str(&format!("  {user}: {}", s.state));
        if let Some(g) = s.in_world {
            out.push_str(&format!(" (in-world guid {g})"));
        }
        if let Some(p) = s.pos {
            out.push_str(&format!(" @ ({:.1}, {:.1}, {:.1})", p[0], p[1], p[2]));
        }
        // Path-movement status (what an agent polls a MoveTo to completion on).
        let m = &s.move_status;
        match m.state {
            MoveState::Moving => out.push_str(&format!(
                " — moving (move #{}, {:.0} yd to go)",
                m.move_id, m.remaining
            )),
            MoveState::Arrived => out.push_str(&format!(" — arrived (move #{})", m.move_id)),
            MoveState::Blocked => out.push_str(&format!(
                " — BLOCKED (move #{}, {:.0} yd short of endpoint)",
                m.move_id, m.remaining
            )),
            MoveState::Idle if s.moving => out.push_str(" walking"),
            MoveState::Idle => {}
        }
        if !s.roster.is_empty() {
            let names: Vec<String> = s
                .roster
                .iter()
                .map(|(g, n, lvl)| format!("{n}(guid {g}, lvl {lvl})"))
                .collect();
            out.push_str(&format!("\n      roster: {}", names.join(", ")));
        }
        out.push('\n');
    }
    out
}
