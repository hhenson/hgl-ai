// hgl-ai console: passkey ceremonies and the answer form on the server-rendered pages.
// Embedded in the binary; the telemetry page is its own (crates/console/web).
"use strict";

function b64urlToBuf(s) {
  s = s.replace(/-/g, "+").replace(/_/g, "/");
  while (s.length % 4) s += "=";
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function bufToB64url(buf) {
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function say(text) {
  const el = document.getElementById("auth-status");
  if (el) el.textContent = text;
}

async function post(url, body) {
  const r = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: body === undefined ? "{}" : JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || ("HTTP " + r.status));
  return data;
}

async function register() {
  say("Waiting for your passkey…");
  const opts = await post("/auth/passkey/register/start");
  const pk = opts.publicKey;
  pk.challenge = b64urlToBuf(pk.challenge);
  pk.user.id = b64urlToBuf(pk.user.id);
  (pk.excludeCredentials || []).forEach((c) => { c.id = b64urlToBuf(c.id); });
  const cred = await navigator.credentials.create({ publicKey: pk });
  const resp = cred.response;
  const done = await post("/auth/passkey/register/finish", {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: bufToB64url(resp.attestationObject),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
      transports: resp.getTransports ? resp.getTransports() : undefined,
    },
    extensions: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
  });
  window.location.assign(done.redirect || "/");
}

async function signin() {
  say("Waiting for your passkey…");
  const opts = await post("/auth/passkey/login/start");
  const pk = opts.publicKey;
  pk.challenge = b64urlToBuf(pk.challenge);
  (pk.allowCredentials || []).forEach((c) => { c.id = b64urlToBuf(c.id); });
  const cred = await navigator.credentials.get({ publicKey: pk });
  const resp = cred.response;
  const done = await post("/auth/passkey/login/finish", {
    id: cred.id,
    rawId: bufToB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: bufToB64url(resp.authenticatorData),
      clientDataJSON: bufToB64url(resp.clientDataJSON),
      signature: bufToB64url(resp.signature),
      userHandle: resp.userHandle ? bufToB64url(resp.userHandle) : null,
    },
    extensions: cred.getClientExtensionResults ? cred.getClientExtensionResults() : {},
  });
  window.location.assign(done.redirect || "/");
}

function wire(id, fn) {
  const b = document.getElementById(id);
  if (!b) return;
  if (!window.PublicKeyCredential) {
    say("This browser has no passkey support.");
    b.disabled = true;
    return;
  }
  b.addEventListener("click", () => {
    b.disabled = true;
    fn().catch((e) => { say("Not signed in: " + e.message); b.disabled = false; });
  });
}

function answerForm() {
  const form = document.querySelector("form.answer");
  if (!form) return;
  const opt = form.querySelector(".when-option");
  const other = form.querySelector(".when-other");
  const sync = () => {
    const c = form.querySelector("input[name=choice]:checked");
    const v = c ? c.value : "";
    if (opt) opt.hidden = v !== "option";
    if (other) other.hidden = v !== "other";
  };
  form.querySelectorAll("input[name=choice]").forEach((r) => r.addEventListener("change", sync));
  sync();
}

document.addEventListener("DOMContentLoaded", () => {
  wire("passkey-register", register);
  wire("passkey-signin", signin);
  answerForm();
});
