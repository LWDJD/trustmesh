// A5 审计数据汇总（读 a5_data/v5_*.txt，输出报告用表）
// 用法：node agg.mjs
import fs from "node:fs";
const dir = new URL("./", import.meta.url);
const read = (f) => {
  const b = fs.readFileSync(new URL(f, dir));
  const s = b.length >= 2 && b[0] === 0xff && b[1] === 0xfe ? b.slice(2).toString("utf16le") : b.toString("utf8");
  return s.replace(/^\uFEFF/, "");
};
const mean = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0);
const sd = (a) => { if (a.length < 2) return 0; const m = mean(a); return Math.sqrt(a.reduce((s, x) => s + (x - m) ** 2, 0) / (a.length - 1)); };
const fmt = (m, s, d = 2) => (m >= 0 ? "+" : "") + m.toFixed(d) + "±" + s.toFixed(d);

// ── 解析 ──
function parse(files) {
  const ev = [], ct = [];
  for (const { file, p } of files) {
    let txt = "";
    try { txt = read(file); } catch { continue; }
    for (const ln of txt.split(/\r?\n/)) {
      if (ln.startsWith("EV,")) {
        const x = ln.split(",");
        ev.push({ p, seed:+x[3], arm:x[4], k:+x[5], obs:+x[6], r0:+x[7], ps:+x[8], pm:+x[9],
          sp:+x[10], hm:+x[11], rho:+x[12], sup:+x[13], live:+x[14], cov:+x[15], s:+x[16], npos:+x[18] });
      } else if (ln.startsWith("CTRL,")) {
        const x = ln.split(",");
        ct.push({ p, seed:+x[3], arm:x[4], r:+x[5], n:+x[6], cs:+x[7], cm:+x[8], cr:+x[9], s50:+x[10], cov50:+x[11] });
      }
    }
  }
  return { ev, ct };
}
const ctrlOk = (ct, p, arm, seed, r) => { const c = ct.find(c => c.p === p && c.arm === arm && c.seed === seed && c.r === r); return c && c.n >= 10 ? c : null; };

// ── 表 E1：A（回归者 − 同轮对照）──
function tableA({ ev, ct }, pList, armOrder, title) {
  console.log("\n## " + title + " —— A = pct(回归者) − 同轮对照均值（百分位；对照 n≥10；跨 seed mean±sd）");
  console.log("p,k,arm,rows,seeds,A_strict,A_mid");
  for (const p of pList) for (let k = 0; k < 10; k++) for (const arm of armOrder) {
    const per = [], perm = []; let rows = 0;
    for (const seed of [...new Set(ev.filter(e => e.p === p && e.arm === arm).map(e => e.seed))].sort((a, b) => a - b)) {
      const rs = ev.filter(e => e.p === p && e.arm === arm && e.seed === seed && e.k === k);
      if (!rs.length) continue;
      const cs = [], cm = [];
      for (const r of [...new Set(rs.map(x => x.r0))]) { const c = ctrlOk(ct, p, arm, seed, r); if (c) { cs.push(c.cs); cm.push(c.cm); } }
      if (!cs.length) continue;
      per.push((mean(rs.map(x => x.ps)) - mean(cs)) * 100);
      perm.push((mean(rs.map(x => x.pm)) - mean(cm)) * 100);
      rows += rs.length;
    }
    if (!per.length) continue;
    console.log([p, k, arm, rows, per.length, fmt(mean(per), sd(per)), fmt(mean(perm), sd(perm))].join(","));
  }
}

// ── 表 E2：配对差 Dp ──
function tableDp({ ev, ct }, pList, armOrder, title) {
  console.log("\n## " + title + " —— 配对差 Dp = pct(臂) − pct(Off)（同 seed/观察者/回归轮；跨 seed mean±sd）");
  console.log("p,k,arm,pairs,seeds,Dp_strict,Dp_mid,win_mid,support_arm,support_off");
  for (const p of pList) for (let k = 0; k < 10; k++) for (const arm of armOrder) {
    if (arm === "off") continue;
    const per = [], perm = [], wins = [], supA = [], supO = []; let pairs = 0;
    for (const seed of [...new Set(ev.filter(e => e.p === p && e.arm === arm).map(e => e.seed))].sort((a, b) => a - b)) {
      const rs = ev.filter(e => e.p === p && e.arm === arm && e.seed === seed && e.k === k);
      const off = ev.filter(e => e.p === p && e.arm === "off" && e.seed === seed && e.k === k);
      const offMap = new Map(off.map(e => [e.obs + ":" + e.r0, e]));
      const d = [], dm = [], ok = [];
      // Dp 是同事件配对差，不依赖对照组（p=90% 时对照 n<10，A 指标按计划判 INVALID，Dp 仍有效）
      for (const r of rs) { const b = offMap.get(r.obs + ":" + r.r0); if (!b) continue; d.push((r.ps - b.ps) * 100); dm.push((r.pm - b.pm) * 100); ok.push(r); }
      if (!d.length) continue;
      per.push(mean(d)); perm.push(mean(dm)); wins.push(dm.filter(x => x > 0).length / dm.length);
      supA.push(mean(ok.map(r => r.sup)));
      supO.push(mean(ok.map(r => offMap.get(r.obs + ":" + r.r0).sup)));
      pairs += d.length;
    }
    if (!per.length) continue;
    console.log([p, k, arm, pairs, per.length, fmt(mean(per), sd(per)), fmt(mean(perm), sd(perm)), mean(wins).toFixed(2), mean(supA).toFixed(1), mean(supO).toFixed(1)].join(","));
  }
}

// ── 表 E3：ρ 分桶 ──
function tableRho({ ev }, pList, armOrder, title) {
  console.log("\n## " + title + " —— 排序质量 Spearman ρ 按覆盖率分桶");
  console.log("p,arm,bucket,rows,seeds,rho_mean,rho_sd,support_mean,s_mean");
  const buckets = [[0, 1e-6, "cov=0"], [1e-6, 0.25, "(0,.25]"], [0.25, 0.5, "(.25,.5]"], [0.5, 0.75, "(.5,.75]"], [0.75, 1.01, "(.75,1]"]];
  for (const p of pList) for (const arm of armOrder) for (const [lo, hi, name] of buckets) {
    const rows = ev.filter(e => e.p === p && e.arm === arm && e.cov >= lo && e.cov < hi && (e.k === 999 || e.k < 10) && e.hm > 0);
    if (!rows.length) continue;
    const per = [];
    for (const seed of [...new Set(rows.map(r => r.seed))]) { const rs = rows.filter(r => r.seed === seed); if (rs.length) per.push(mean(rs.map(r => r.rho))); }
    console.log([p, arm, name, rows.length, per.length, mean(per).toFixed(4), sd(per).toFixed(4), mean(rows.map(r => r.sup)).toFixed(1), mean(rows.map(r => r.s)).toFixed(3)].join(","));
  }
}

// ── 表 E4：在线观察者的 s / 支持集 ──
function tableS({ ev }, pList, armOrder) {
  console.log("\n## 表 E4 在线观察者的 s 与支持集（k=999 行，8 seed）");
  console.log("p,arm,rows,s_mean,s_p10,s_p50,s_p90,support_mean,sup_vs_off");
  for (const p of pList) {
    let off = 0;
    for (const arm of armOrder) {
      const rows = ev.filter(e => e.p === p && e.arm === arm && e.k === 999 && e.hm > 0);
      if (!rows.length) continue;
      const sv = rows.map(r => r.s).sort((a, b) => a - b);
      const sup = mean(rows.map(r => r.sup));
      if (arm === "off") off = sup;
      console.log([p, arm, rows.length, mean(sv).toFixed(3), sv[Math.floor(sv.length * 0.1)].toFixed(3), sv[Math.floor(sv.length * 0.5)].toFixed(3), sv[Math.floor(sv.length * 0.9)].toFixed(3), sup.toFixed(1), off ? (sup / off).toFixed(3) : "-"].join(","));
    }
  }
}

// ── 表 S1/S3 ──
function tableS13() {
  console.log("\n## 表 S1/S3 真 Engine 复核（5 seed）");
  console.log("kind,arm,seeds,ratio_mean,ratio_sd,min,max");
  const rows = new Map();
  for (const [f, kind] of [["v5_s1_sybil.txt", "sybil"], ["v5_s3_sleeper.txt", "sleeper"], ["v5_s3_infiltrator.txt", "infiltrator"], ["v5_s3_endorser.txt", "endorser"]]) {
    let t; try { t = read(f); } catch { continue; }
    for (const ln of t.split(/\r?\n/)) {
      if (ln.startsWith("S1,")) { const x = ln.split(","); const k = kind + "|" + x[2]; if (!rows.has(k)) rows.set(k, []); rows.get(k).push(+x[6]); }
      else if (ln.startsWith("S3,")) { const x = ln.split(","); const k = x[1] + "|" + x[3]; if (!rows.has(k)) rows.set(k, []); rows.get(k).push(+x[5]); }
    }
  }
  for (const [k, v] of [...rows.entries()].sort()) {
    const [kind, arm] = k.split("|");
    console.log([kind, arm, v.length, mean(v).toFixed(3), sd(v).toFixed(3), Math.min(...v).toFixed(3), Math.max(...v).toFixed(3)].join(","));
  }
}

// ── 执行 ──
const effect = parse(["v5_effect_p030.txt","v5_effect_p050.txt","v5_effect_p070.txt","v5_effect_p090.txt"].map((file,i)=>({file,p:[0.3,0.5,0.7,0.9][i]})));
const attrib = parse([{file:"v5_attrib_p070.txt",p:0.7},{file:"v5_attrib_p090.txt",p:0.9}]);
console.log("# A5 汇总（数据：v5_*，8 seed，D=216h，攻击=Sleeper 1/300）");
console.log("effect: EV " + effect.ev.length + " / CTRL " + effect.ct.length + "；attrib: EV " + attrib.ev.length);
tableA(effect, [0.3,0.5,0.7,0.9], ["off","cov","g"], "表 E1 主对比 Off/Cov/G");
tableDp(effect, [0.3,0.5,0.7,0.9], ["off","cov","g"], "表 E2 主对比");
tableA(attrib, [0.7,0.9], ["off","gs","gsr","g"], "表 E5 归因 A");
tableDp(attrib, [0.7,0.9], ["off","gs","gsr","g"], "表 E6 归因 Dp");
tableRho(effect, [0.3,0.5,0.7,0.9], ["off","cov","g"], "表 E3");
tableS(effect, [0.3,0.5,0.7,0.9], ["off","cov","g"]);
tableS13();
console.log("\n## 表 D1 live 组成（v2 diag，8 seed；native 与 att-online 两种可达性）");
console.log("mode,p,k,rows,live,n_pos,cov,live_slp%,pop_slp%,pos_slp%,ratio_pop,ratio_online");
for (const [f, mode] of [["v2_diag_p050.txt","native"],["v2_diag_p070.txt","native"],["v2_diag_p090.txt","native"],["v2_diagadv_p070.txt","adv"],["v2_diagadv_p090.txt","adv"]]) {
  const g = new Map();
  for (const ln of read(f).split(/\r?\n/)) {
    if (!ln.startsWith("DIAG,")) continue;
    const x = ln.split(","); const k = +x[4];
    if (!g.has(k)) g.set(k, []);
    g.get(k).push({ n:+x[5], live:+x[6], npos:+x[7], cov:+x[8], lslp:+x[10], pslp:+x[12], poslp:+x[14], rs:+x[16], rso:+x[18] });
  }
  for (const [k, v] of [...g.entries()].sort((a,b)=>a[0]-b[0])) {
    console.log([mode, +v.length ? "" : "", k, v.reduce((s,x)=>s+x.n,0), mean(v.map(x=>x.live)).toFixed(2), mean(v.map(x=>x.npos)).toFixed(2), mean(v.map(x=>x.cov)).toFixed(3), (mean(v.map(x=>x.lslp))*100).toFixed(1), (mean(v.map(x=>x.pslp))*100).toFixed(1), (mean(v.map(x=>x.poslp))*100).toFixed(1), mean(v.map(x=>x.rs)).toFixed(2), mean(v.map(x=>x.rso)).toFixed(2)].filter((_,i)=>i!==1).join(","));
  }
}
