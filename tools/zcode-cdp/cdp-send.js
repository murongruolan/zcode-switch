// 通过 CDP 在 ZCode 主窗口输入一条消息并回车发送
const TEXT = process.argv[2] || "只回复 OK";
async function main() {
  const list = await (await fetch("http://127.0.0.1:9229/json/list")).json();
  const page = list.find(t => t.type === "page" && /ZCode/.test(t.title)) || list.find(t => t.type === "page");
  if (!page) { console.log("no page target"); return; }
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

  // 1) 聚焦输入框
  const focus = await send("Runtime.evaluate", {
    expression: `(() => {
      const el = [...document.querySelectorAll('[contenteditable="true"]')].find(e => e.offsetParent !== null);
      if (!el) return 'no editable';
      el.focus();
      const r = el.getBoundingClientRect();
      return JSON.stringify({ok:true, x: Math.round(r.x + r.width/2), y: Math.round(r.y + r.height/2)});
    })()`,
    returnByValue: true
  });
  console.log("focus:", JSON.stringify(focus.result?.result?.value));
  let pos = { x: 600, y: 700 };
  try { const v = JSON.parse(focus.result.result.value); if (v.ok) pos = v; } catch (e) {}

  // 2) 点击一次确保焦点在输入区
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
  await new Promise(r => setTimeout(r, 300));

  // 3) 插入文本（走浏览器输入管线，React 能收到）
  await send("Input.insertText", { text: TEXT });
  await new Promise(r => setTimeout(r, 500));

  // 4) 校验文本已进入
  const chk = await send("Runtime.evaluate", {
    expression: `(() => {
      const el = [...document.querySelectorAll('[contenteditable="true"]')].find(e => e.offsetParent !== null);
      return el ? (el.innerText || '').slice(0, 200) : 'gone';
    })()`,
    returnByValue: true
  });
  console.log("input now:", JSON.stringify(chk.result?.result?.value));

  // 5) 回车发送
  for (const t of ["rawKeyDown", "char", "keyUp"]) {
    await send("Input.dispatchKeyEvent", {
      type: t, key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13,
      text: t === "char" ? "\r" : undefined
    });
  }
  console.log("enter sent");
  await new Promise(r => setTimeout(r, 1500));
  const after = await send("Runtime.evaluate", {
    expression: `(() => {
      const el = [...document.querySelectorAll('[contenteditable="true"]')].find(e => e.offsetParent !== null);
      return el ? ('left:' + (el.innerText||'').length) : 'gone';
    })()`,
    returnByValue: true
  });
  console.log("after:", JSON.stringify(after.result?.result?.value));
  ws.close();
}
main().catch(e => console.log("ERR", e.message));
