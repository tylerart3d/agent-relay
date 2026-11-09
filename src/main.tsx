import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import ModelMenu from "./ModelMenu";
import NoticeWindow from "./NoticeWindow";
import TrayMenu from "./TrayMenu";

const windowLabel = getCurrentWindow().label;
const Root = windowLabel === "tray-menu" ? TrayMenu
  : windowLabel === "model-menu" ? ModelMenu
  : windowLabel === "notice" ? NoticeWindow
  : App;
const floating = windowLabel.endsWith("-menu") || windowLabel === "notice";
document.body.classList.add(floating ? "tray-window" : "status-window");
// Windows rounds and borders these windows natively; macOS needs CSS rounding.
document.body.classList.add(/Mac/i.test(navigator.userAgent) ? "platform-macos" : "platform-windows");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
