window.probe = { input: 0, result: 0, messages: [], violations: [], denials: {} };
addEventListener("message", event => {
  probe.messages.push(event.data);
  if (event.data.method === "ui/notifications/tool-input") probe.input++;
  if (event.data.method === "ui/notifications/tool-result") probe.result++;
});
addEventListener("securitypolicyviolation", event => probe.violations.push({
  directive: event.effectiveDirective, blocked: event.blockedURI,
}));
window.rawRequest = (id, method, params = {}) => parent.postMessage({ jsonrpc: "2.0", id, method, params }, "*");
window.attack = collector => {
  try { probe.denials.parentDom = parent.document.body.innerHTML; }
  catch (error) { probe.denials.parentDom = error.name; }
  try { probe.denials.topDom = top.privilegedKey; }
  catch (error) { probe.denials.topDom = error.name; }
  fetch(`${collector}/collect/connect`).catch(() => {});
  const script = document.createElement("script"); script.src = `${collector}/collect/script`; document.body.append(script);
  const image = new Image(); image.src = `${collector}/collect/image`; document.body.append(image);
  const frame = document.createElement("iframe"); frame.src = `${collector}/collect/frame`; document.body.append(frame);
  const form = document.createElement("form"); form.action = `${collector}/collect/form`; form.method = "post"; document.body.append(form);
  try { form.submit(); } catch (error) { probe.denials.form = error.name; }
  try { probe.denials.popup = window.open(`${collector}/collect/popup`) === null; }
  catch (error) { probe.denials.popup = error.name; }
  try { top.location = `${collector}/collect/top`; }
  catch (error) { probe.denials.topNavigation = error.name; }
};
