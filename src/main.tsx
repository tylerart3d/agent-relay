import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import ModelMenu from "./ModelMenu";
import TrayMenu from "./TrayMenu";

const windowLabel = getCurrentWindow().label;
const Root = windowLabel === "tray-menu" ? TrayMenu : windowLabel === "model-menu" ? ModelMenu : App;
document.body.classList.add(windowLabel.endsWith("-menu") ? "tray-window" : "status-window");

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
