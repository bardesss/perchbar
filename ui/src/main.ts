import { mount } from "svelte";
import App from "./App.svelte";
import "./theme.css";

declare global {
  interface Window {
    /** Set by the core when no Mica backdrop shows through the window. */
    __PERCHBAR_SOLID_BACKDROP__?: boolean;
  }
}

if (window.__PERCHBAR_SOLID_BACKDROP__) {
  document.documentElement.classList.add("solid-backdrop");
}

const target = document.getElementById("app");
if (!target) throw new Error("missing #app");

export default mount(App, { target });
