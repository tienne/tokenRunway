import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type UpdateStatus = { available: string | null; installing: boolean };

/** 받을 수 있는 새 버전과 설치 중인지. 백엔드가 6시간마다 확인하고 바뀌면 알려준다. */
export function useUpdateStatus(): UpdateStatus {
  const [status, setStatus] = useState<UpdateStatus>({ available: null, installing: false });
  useEffect(() => {
    invoke<UpdateStatus>("get_update_status").then(setStatus).catch(() => {});
    const un = listen<UpdateStatus>("update-status", (e) => setStatus(e.payload));
    return () => {
      un.then((f) => f()).catch(() => {});
    };
  }, []);
  return status;
}
