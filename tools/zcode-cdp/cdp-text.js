// 读取指定端口 ZCode 窗口的文本片段，可选 grep 关键字
const PORT = process.argv[2] || "9229";
const KW = process.argv[3] || "";
const TAIL = parseInt(process.argv[4] || "4000", 10);

async function main() {
  const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  const page = list.find(t => t.type === "page" && /ZCode/.test(t.title)) || list.find(t => t.type === "page");
  if (!page) { console.log("no page"); return; }
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  let id = 0; const pending = new Map();
  const send = (m, p) => new Promise(r => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method: m, params: p })); });
  ws.addEventListener("message", ev => { const m = JSON.parse(ev.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } });
  await new Promise(r => ws.addEventListener("open", r));
  const expr = `(() => { const t = document.body.innerText || ''; return t.slice(-${TAIL}); })()`;
  const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true });
  let txt = r.result?.result?.value || "";
  if (KW) {
    const lines = txt.split("\n").filter(l => l.includes(KW));
    console.log(lines.length ? lines.join("\n") : `(no line contains "${KW}")`);
  } else {
    console.log(txt);
  }
  ws.close();
}
main().catch(e => console.log("ERR", e.message));
