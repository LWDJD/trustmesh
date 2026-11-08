#![allow(irrefutable_let_patterns)]

//! 多节点运行时端到端测试。
//!
//! 验证：A 信任 B，B 信任 C，A 是否间接信任 C。

use trustmesh::presets::Preset;
use trustmesh::runtime::{account, run_loop, MemoryTransport, Runtime, RuntimeConfig};

fn node(id: u64, peers: Vec<u64>) -> Runtime {
    let mut cfg = RuntimeConfig::default();
    cfg.preset = Preset::recommended();
    cfg.peers = peers;
    cfg.publish_every = 5;
    cfg.samples_per_tick = 10;
    Runtime::new(id, cfg, id | 1)
}

#[test]
fn indirect_trust_propagates() {
    // 节点 ID 用哈希生成，保证池内均匀
    let a_id = account(b"node-a");
    let b_id = account(b"node-b");
    let c_id = account(b"node-c");

    // A → B → C
    let mut a = node(a_id, vec![b_id]);
    let mut b = node(b_id, vec![a_id, c_id]);
    let mut c = node(c_id, vec![b_id]);

    // A 信任 B，B 信任 C
    a.rate(b_id, 1.0);
    b.rate(c_id, 1.0);
    c.rate(b_id, 1.0);

    // 手动驱动三轮，交换消息
    for t in 0..30u64 {
        a.tick(t);
        b.tick(t);
        c.tick(t);

        let a_out = a.take_outbox();
        let b_out = b.take_outbox();
        let c_out = c.take_outbox();

        for act in a_out {
            if let trustmesh::runtime::Action::Send { to, msg } = act {
                if to == b_id {
                    b.on_message(a_id, msg);
                }
            }
        }
        for act in b_out {
            if let trustmesh::runtime::Action::Send { to, msg } = act {
                match to {
                    x if x == a_id => a.on_message(b_id, msg),
                    x if x == c_id => c.on_message(b_id, msg),
                    _ => {}
                }
            }
        }
        for act in c_out {
            if let trustmesh::runtime::Action::Send { to, msg } = act {
                if to == b_id {
                    b.on_message(c_id, msg);
                }
            }
        }
    }

    let qc = a.query(c_id);
    // A 自己没给 C 打分（默认 0.5），但 B 信任 C
    // 期望：q > 0.5
    assert!(
        qc > 0.5,
        "A 应通过 B 间接信任 C，实际 {}",
        qc
    );
}

#[test]
fn run_loop_with_memory_transport() {
    let mut rt = node(1, vec![]);
    rt.rate(100, 0.9);
    let mut tp = MemoryTransport::new();
    run_loop(&mut rt, &mut tp, 10);
    // 没有 peers，不应产生消息
    assert!(tp.sent.is_empty());
}

#[test]
fn summary_request_roundtrip() {
    let mut a = node(1, vec![2]);
    let mut b = node(2, vec![1]);

    // A 向 B 发请求：模拟 B 收到并入站
    b.rate(100, 0.9);
    // A 必须信任 B（给 B 打分），B 才会进入抽样池
    a.rate(2, 1.0);

    // B 收到来自 A 的请求，应产生回应
    b.on_message(1, trustmesh::runtime::Message::RequestSummary { target: 100, at: 0 });
    let b_out = b.take_outbox();
    assert_eq!(b_out.len(), 1, "收到请求应回应");

    // A 收到回应
    for act in b_out {
        if let trustmesh::runtime::Action::Send { msg, .. } = act {
            a.on_message(2, msg);
        }
    }

    let q = a.query(100);
    assert!(q > 0.5, "A 应受 B 的摘要影响，实际 {}", q);
}
