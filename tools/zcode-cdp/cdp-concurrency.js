// 用 CDP 在 ZCode 里发任务，然后周期性采样 subagent 状态
const TEXT = process.argv[2];
const SECONDS = parseInt(process.argv[3] || "120", 10);
const PORT = process.argv[4] || "9229";

async function cdp() {
  const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  const page = list.find(t => t.type === "page" && /ZCode/.test(t.title)) || list.find(t => t.type === "page");
  if (!page) throw new Error("no page target");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  let id = 0;
  const pending = new Map();
  const send = (method, params) => new Promise((res) => {
    const msgId = ++id;
    pending.set(msgId, res);
    ws.send(JSON.stringify({ id: msgId, method, params }));
  });
  ws.addEventListener("message", (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  });
  await new Promise((r) => ws.addEventListener("open", r));
  return { ws, send };
}

const STAT = `(() => {
  const txt = document.body.innerText || '';
  const subLines = (txt.match(/子智能体/g) || []).length;
  const fail = (txt.match(/执行失败/g) || []).length;
  const running = (txt.match(/运行中|执行中|思考中/g) || []).length;
  const done = (txt.match(/已完成|已工作/g) || []).length;
  const busy = (txt.match(/系统繁忙|已达到最大次数|被挤/g) || []).length;
  return JSON.stringify({sub: subLines, fail, running, done, busy, chars: txt.length});
})()`;

async function main() {
  const { ws, send } = await cdp();
  if (TEXT && TEXT !== "-") {
    // focus + click + insert + enter
    const f = await send("Runtime.evaluate", {
      expression: `(() => { const el=[...document.querySelectorAll('[contenteditable="true"]')].find(e=>e.offsetParent!==null); if(!el) return 'no'; el.focus(); const r=el.getBoundingClientRect(); return JSON.stringify({x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)}); })()`,
      returnByValue: true
    });
    let pos = { x: 600, y: 700 };
    try { pos = JSON.parse(f.result.result.value); } catch (e) {}
    await send("Input.dispatchMouseEvent", { type: "mousePressed", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
    await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
    await new Promise(r => setTimeout(r, 250));
    await send("Input.insertText", { text: TEXT });
    await new Promise(r => setTimeout(r, 400));
    for (const t of ["rawKeyDown", "char", "keyUp"]) {
      await send("Input.dispatchKeyEvent", { type: t, key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13, text: t === "char" ? "\r" : undefined });
    }
    console.log("[sent] " + TEXT.slice(0, 60));
  }
  const t0 = Date.now();
  while ((Date.now() - t0) / 1000 < SECONDS) {
    await new Promise(r => setTimeout(r, 15000));
    const s = await send("Runtime.evaluate", { expression: STAT, returnByValue: true });
    const v = s.result?.result?.value;
    console.log(`[${Math.round((Date.now() - t0) / 1000)}s] ${v}`);
  }
  ws.close();
}
main().catch(e => console.log("ERR", e.message));
