import { invoke } from "@tauri-apps/api/core";
import { useEffect } from "react";

export interface Notice { key: string; title: string; message: string; at_ms: number }

/** Shows `message` in the separate notice window while it is set; null clears it.
 *  Each `key` owns one notice, so repeated errors replace rather than stack. */
export function useNotice(key: string, title: string, message: string | null | undefined) {
  useEffect(() => {
    void invoke("set_notice", { key, title, message: message || null }).catch(() => undefined);
  }, [key, title, message]);
}
