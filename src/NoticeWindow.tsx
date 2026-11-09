import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { Notice } from "./notices";

export default function NoticeWindow() {
  const [notices, setNotices] = useState<Notice[]>([]);
  const shellRef = useRef<HTMLElement>(null);
  const lastHeight = useRef(0);

  useEffect(() => {
    let active = true;
    const read = () => invoke<Notice[]>("get_notices")
      .then((next) => { if (active) setNotices(next ?? []); })
      .catch(() => undefined);
    void read();
    const unlisten = listen("notices-changed", () => { void read(); });
    return () => { active = false; void unlisten.then((stop) => stop()); };
  }, []);

  // The window is sized to its content so it never covers more of the screen than needed.
  useLayoutEffect(() => {
    const shell = shellRef.current;
    if (!shell) return;
    const height = Math.ceil(shell.getBoundingClientRect().height);
    if (height > 0 && height !== lastHeight.current) {
      lastHeight.current = height;
      void invoke("resize_notice", { height }).catch(() => undefined);
    }
  });

  const dismiss = (key?: string) => void invoke("dismiss_notice", { key: key ?? null }).catch(() => undefined);

  return (
    <section className="notice-shell" ref={shellRef} aria-label="Agent Relay notices">
      <header className="notice-header">
        <strong>Agent Relay</strong>
        {notices.length > 1 && <button className="notice-dismiss" onClick={() => dismiss()}>Dismiss all</button>}
      </header>
      <ul className="notice-list">
        {notices.map((notice) => (
          <li className="notice-item" key={notice.key} role="alert">
            <div>
              <strong>{notice.title}</strong>
              <p>{notice.message}</p>
            </div>
            <button className="notice-dismiss" aria-label={`Dismiss ${notice.title}`} onClick={() => dismiss(notice.key)}>Dismiss</button>
          </li>
        ))}
      </ul>
    </section>
  );
}
