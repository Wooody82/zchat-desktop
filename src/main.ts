import { invoke } from "@tauri-apps/api/core";

window.addEventListener("DOMContentLoaded", () => {
  const form = document.querySelector<HTMLFormElement>("#workspace-form")!;
  const input = document.querySelector<HTMLInputElement>("#url-input")!;
  const errorMsg = document.querySelector<HTMLElement>("#error-msg")!;
  const btn = document.querySelector<HTMLButtonElement>("#submit-btn")!;

  input.focus();

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const url = input.value.trim();
    if (!url) return;

    errorMsg.textContent = "";
    btn.textContent = "Connecting...";
    btn.disabled = true;

    try {
      await invoke("save_workspace", { url });
    } catch (err) {
      errorMsg.textContent = String(err);
      btn.textContent = "Open Workspace";
      btn.disabled = false;
    }
  });
});
