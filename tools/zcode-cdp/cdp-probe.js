// CDP 探测：连 ZCode 主窗口，dump 输入区结构
const EXPR = process.argv[2] || `(() => {
  const tas = [...document.querySelectorAll('textarea')].map(t => ({tag:'textarea', ph:t.placeholder, cls:String(t.className).slice(0,60), rows:t.rows, vis:t.offsetParent!==null}));
  const ces = [...document.querySelectorAll('[contenteditable="true"]')].map(t => ({tag:t.tagName, cls:String(t.className).slice(0,60), ph:t.getAttribute('data-placeholder')||t.getAttribute('aria-label')||'', vis:t.offsetParent!==null}));
  return JSON.stringify({url: location.href.slice(0,80), textareas: tas, editables: ces}, null, 1);
})()`;

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
  const r = await send("Runtime.evaluate", { expression: EXPR, returnByValue: true, awaitPromise: true });
  console.log(JSON.stringify(r.result, null, 1));
  ws.close();
}
main().catch(e => console.log("ERR", e.message));
