//! 推送前裁剪：数量与字段长度对齐 hub 的 sanitize() 上限，并保证序列化后的请求体不超过上限。
//!
//! 数量：sources ≤16（被截掉档位的 leads/events 一并去掉）、leads ≤300、每 lead agents ≤200、
//! 全设备 agents ≤2000（working 优先，再按 last 倒序）、events ≤400。
//! 字段：去掉控制/Cf 字符后按 Unicode 字符截断（同 hub 的 _s 上限）。
//! 体积超限时依次：删 RECENT lead（连同事件）→ 从旧到新删非 working 的 agents（连同事件）
//! → 清空 reply → 清空 task → events 减半 → 只发 sources + working/idle lead 骨架（保留 tool）。

use std::collections::HashSet;

use serde_json::Value;

use crate::scan::State;
use crate::text::clean;

/// 实际推送内容的规模（用于日志）
#[derive(Default, Clone, Copy, Debug)]
pub struct Sizes {
    pub bytes: usize,
    pub leads: usize,
    pub agents: usize,
    pub events: usize,
}

/// 文本型 Value：字符串清洗截断；null 保留；其他类型 hub 也会丢弃，直接置 null
fn cv(v: &mut Value, n: usize) {
    *v = match v {
        Value::String(s) => Value::String(clean(s, n)),
        _ => Value::Null,
    };
}

fn cs(s: &mut String, n: usize) {
    *s = clean(s, n);
}

fn co(s: &mut Option<String>, n: usize) {
    if let Some(x) = s {
        *x = clean(x, n);
    }
}

fn clamp_fields(st: &mut State) {
    for s in &mut st.sources {
        cv(&mut s.label, 40);
    }
    for l in &mut st.leads {
        cs(&mut l.id, 64);
        cs(&mut l.name, 32);
        cs(&mut l.face, 32);
        cv(&mut l.title, 160);
        cs(&mut l.project, 80);
        cv(&mut l.branch, 80);
        cv(&mut l.model, 60);
        cv(&mut l.reply, 400);
        cv(&mut l.tool, 40);
        for a in &mut l.agents {
            cs(&mut a.id, 64);
            cs(&mut a.name, 32);
            cv(&mut a.typ, 40);
            cv(&mut a.task, 160);
            cv(&mut a.model, 60);
            cv(&mut a.reply, 400);
            cv(&mut a.tool, 40);
        }
    }
    for e in &mut st.events {
        cs(&mut e.lid, 64);
        co(&mut e.aid, 64);
        co(&mut e.who, 32);
        co(&mut e.lead, 32);
        cv(&mut e.job, 40);
        cv(&mut e.task, 160);
    }
}

fn is_recent(status: &str) -> bool {
    !matches!(status, "working" | "idle")
}

fn encode(device: &str, st: &State) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"device": device, "state": st})).unwrap_or_default()
}

/// 按 (位置) 集合删除 agents，并去掉这些 agent 的事件
fn drop_agents(st: &mut State, kill: &HashSet<(usize, usize)>) {
    let mut gone: HashSet<(String, String, String)> = HashSet::new();
    for (li, l) in st.leads.iter_mut().enumerate() {
        let mut ai = 0;
        let (src, lid) = (l.src.clone(), l.id.clone());
        l.agents.retain(|a| {
            let k = kill.contains(&(li, ai));
            ai += 1;
            if k {
                gone.insert((src.clone(), lid.clone(), a.id.clone()));
            }
            !k
        });
    }
    if !gone.is_empty() {
        st.events.retain(|e| match &e.aid {
            Some(aid) => !gone.contains(&(e.src.clone(), e.lid.clone(), aid.clone())),
            None => true,
        });
    }
}

pub fn trim_and_encode(device: &str, mut st: State, max: usize) -> (Vec<u8>, Sizes) {
    // ---- 数量上限
    st.sources.truncate(16);
    let ids: HashSet<String> = st.sources.iter().map(|s| s.id.clone()).collect();
    st.leads.retain(|l| ids.contains(&l.src));
    st.events.retain(|e| ids.contains(&e.src));
    st.leads.truncate(300);
    for l in &mut st.leads {
        l.agents.truncate(200);
    }
    let mut all: Vec<(usize, usize, bool, i64)> = Vec::new();
    for (li, l) in st.leads.iter().enumerate() {
        for (ai, a) in l.agents.iter().enumerate() {
            all.push((li, ai, a.status == "working", a.last));
        }
    }
    if all.len() > 2000 {
        all.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.cmp(&a.3)));
        let kill: HashSet<(usize, usize)> = all.iter().skip(2000).map(|x| (x.0, x.1)).collect();
        drop_agents(&mut st, &kill);
    }
    st.events.truncate(400);
    clamp_fields(&mut st);

    let mut body = encode(device, &st);

    // 1) 删 RECENT 会话（leads 已按状态排序，RECENT 在末尾），每轮约 10%
    while body.len() > max {
        let recent = st.leads.iter().filter(|l| is_recent(l.status)).count();
        if recent == 0 {
            break;
        }
        let n = recent.div_ceil(10);
        let mut dropped: HashSet<(String, String)> = HashSet::new();
        let mut i = st.leads.len();
        while i > 0 && dropped.len() < n {
            i -= 1;
            if is_recent(st.leads[i].status) {
                let l = st.leads.remove(i);
                dropped.insert((l.src, l.id));
            }
        }
        st.events.retain(|e| !dropped.contains(&(e.src.clone(), e.lid.clone())));
        body = encode(device, &st);
    }

    // 2) 从旧到新删非 working 的 agents，每轮约 10%
    while body.len() > max {
        let mut cand: Vec<(i64, usize, usize)> = Vec::new();
        for (li, l) in st.leads.iter().enumerate() {
            for (ai, a) in l.agents.iter().enumerate() {
                if a.status != "working" {
                    cand.push((a.last, li, ai));
                }
            }
        }
        if cand.is_empty() {
            break;
        }
        cand.sort();
        let n = cand.len().div_ceil(10);
        let kill: HashSet<(usize, usize)> = cand.iter().take(n).map(|x| (x.1, x.2)).collect();
        drop_agents(&mut st, &kill);
        body = encode(device, &st);
    }

    // 3) 清空 reply，再清空 task
    if body.len() > max {
        for l in &mut st.leads {
            l.reply = Value::Null;
            for a in &mut l.agents {
                a.reply = Value::Null;
            }
        }
        body = encode(device, &st);
    }
    if body.len() > max {
        for l in &mut st.leads {
            for a in &mut l.agents {
                a.task = Value::Null;
            }
        }
        for e in &mut st.events {
            e.task = Value::Null;
        }
        body = encode(device, &st);
    }

    // 4) events 减半
    while body.len() > max && !st.events.is_empty() {
        let n = st.events.len() / 2;
        st.events.truncate(n);
        body = encode(device, &st);
    }

    // 5) 兜底：只发 sources + working/idle lead 骨架（无 agents）
    if body.len() > max {
        st.events.clear();
        st.leads.retain(|l| !is_recent(l.status));
        for l in &mut st.leads {
            l.agents.clear();
            l.title = Value::Null;
            l.branch = Value::Null;
            l.reply = Value::Null;
        }
        body = encode(device, &st);
    }
    // 理论上骨架必然满足上限；仍超限则从末尾删 lead，绝不原样推送
    while body.len() > max && !st.leads.is_empty() {
        st.leads.pop();
        body = encode(device, &st);
    }

    let sizes = Sizes {
        bytes: body.len(),
        leads: st.leads.len(),
        agents: st.leads.iter().map(|l| l.agents.len()).sum(),
        events: st.events.len(),
    };
    (body, sizes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Agent, Event, Lead, Num, SrcStat};

    const MAX: usize = 900 * 1024;

    fn agent(i: usize, status: &'static str, reply: &str) -> Agent {
        Agent {
            id: format!("a{i:016x}"),
            name: "Ada".into(),
            typ: Value::String("general-purpose".into()),
            task: Value::String("任".repeat(300)),
            model: Value::String("claude-opus".into()),
            bg: false,
            status,
            start: 1_700_000_000_000 + i as i64,
            last: 1_700_000_000_000 + i as i64,
            reply: Value::String(reply.into()),
            tool: Value::String("Bash".into()),
        }
    }

    fn lead(i: usize, status: &'static str, agents: Vec<Agent>) -> Lead {
        Lead {
            id: format!("lead-{i}"),
            src: "cli".into(),
            name: "Nova".into(),
            title: Value::String("标".repeat(500)),
            project: "p".repeat(200),
            branch: Value::String("b".repeat(200)),
            model: Value::String("m".repeat(200)),
            pid: Value::Null,
            status,
            start: 1,
            last: Num::I(1_700_000_000_000 + i as i64),
            reply: Value::String("回".repeat(1000)),
            tool: Value::String("t".repeat(100)),
            agents,
            face: "Nova".into(),
        }
    }

    fn state(leads: Vec<Lead>, events: Vec<Event>) -> State {
        State {
            now: 1,
            hours: 12.0,
            host: "h".into(),
            sources: vec![SrcStat { id: "cli".into(), label: Value::String("claude".into()), dir: "~/.claude".into(), online: 0, working: 0, sessions: 0, agents: 0 }],
            leads,
            events,
        }
    }

    #[test]
    fn many_working_agents_fit() {
        // 1500 个 working agent，reply 满长（400 个中文字符）
        let reply = "答".repeat(400);
        let mut leads = Vec::new();
        for li in 0..8 {
            let agents = (0..188).map(|i| agent(li * 1000 + i, "working", &reply)).collect();
            leads.push(lead(li, "working", agents));
        }
        let total: usize = leads.iter().map(|l| l.agents.len()).sum();
        assert!(total >= 1500);
        let events = (0..400)
            .map(|i| Event {
                src: "cli".into(),
                lid: "lead-0".into(),
                aid: Some(format!("a{i:016x}")),
                who: Some("Ada".into()),
                job: Value::String("agent".into()),
                task: Value::String("务".repeat(500)),
                at: Num::I(i),
                kind: "on",
                lead: Some("Nova".into()),
            })
            .collect();
        let st = state(leads, events);
        assert!(encode("dev", &st).len() > MAX);
        let (body, sizes) = trim_and_encode("dev", st, MAX);
        assert!(body.len() <= MAX, "body {} > {}", body.len(), MAX);
        assert_eq!(sizes.agents, total, "working agents 不应被删");
        // 字段已按上限截断
        let v: Value = serde_json::from_slice(&body).unwrap();
        let l0 = &v["state"]["leads"][0];
        assert!(l0["title"].as_str().is_none_or(|s| s.chars().count() <= 160));
        assert!(l0["project"].as_str().unwrap().chars().count() <= 80);
    }

    #[test]
    fn skeleton_fallback_never_oversize() {
        // 极小上限：必须走到骨架/删 lead，而不是原样推送
        let leads = (0..50).map(|i| lead(i, if i % 2 == 0 { "working" } else { "done" }, vec![agent(i, "finished", "x")])).collect();
        let (body, _) = trim_and_encode("dev", state(leads, Vec::new()), 4096);
        assert!(body.len() <= 4096);
    }

    #[test]
    fn sources_cap_filters_leads() {
        let mut st = state(vec![lead(0, "working", vec![])], Vec::new());
        st.sources.clear();
        for i in 0..20 {
            st.sources.push(SrcStat { id: format!("s{i}"), label: Value::Null, dir: String::new(), online: 0, working: 0, sessions: 0, agents: 0 });
        }
        st.leads[0].src = "s19".into();
        let (_, sizes) = trim_and_encode("dev", st, MAX);
        assert_eq!(sizes.leads, 0);
    }
}
