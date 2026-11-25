import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import App from "./App";
import "./index.css";

/** The PWA shell (`/sw.js`, embedded in `spawn serve`,
 * crates/spawn-cli/src/serve/pwa.rs): registered once the page has loaded,
 * in a build only (the dev server's modules are not hashed assets), and only
 * where service workers exist and the page is a secure context (https, or
 * localhost). A failure leaves the page as it is: online, uncached. */
function registerServiceWorker(): void {
  if (!import.meta.env.PROD || !("serviceWorker" in navigator) || !window.isSecureContext) return;
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js", { scope: "/" }).catch((error: unknown) => {
      console.warn("spawn: the service worker was not registered", error);
    });
  });
}

registerServiceWorker();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
