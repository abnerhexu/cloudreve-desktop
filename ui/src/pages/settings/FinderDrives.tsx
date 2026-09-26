import { Alert, Box, Button, Stack, Typography } from "@mui/material";
import { invoke } from "@tauri-apps/api/core";
import { openPath } from "@tauri-apps/plugin-opener";
import { ask } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

type Domain = {id: string; name: string; writable: boolean; server?: string; root?: string;
  user_enabled: boolean; disconnected: boolean; credentials_accessible: boolean; can_reauthorize?: boolean;
  error_domain?: string; error_code?: number};
export default function FinderDrives({onCount}: {onCount: (count: number) => void}) {
  const {t} = useTranslation();
  const [domains, setDomains] = useState<Domain[]>([]);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const refresh = useCallback(async () => {
    try { const result = await invoke<Domain[]>("list_finder_drives"); setDomains(result); onCount(result.length); }
    catch (e) { setMessage(String(e)); }
  }, [onCount]);
  useEffect(() => {
    refresh();
    const focus = () => { refresh(); };
    window.addEventListener("focus", focus);
    return () => window.removeEventListener("focus", focus);
  }, [refresh]);
  const recover = async (domain: Domain, reauthorize: boolean) => {
    setBusy(true); setMessage("");
    try {
      if (reauthorize) await invoke("show_reauthorize_window", {driveId: domain.id, siteUrl: domain.server, driveName: domain.name, finder: true});
      else { await invoke("resume_finder_drive", {domainId: domain.id}); setMessage(t("finder.retryRequested")); }
      await refresh();
    } catch (e) { setMessage(String(e)); }
    finally { setBusy(false); }
  };
  const open = async (id: string) => {
    try { setMessage(""); const location = await invoke<{path: string}>("finder_drive_location", {domainId: id}); await openPath(location.path); }
    catch (e) { setMessage(String(e)); }
  };
  const remove = async (domain: Domain) => {
    if (!await ask(t("finder.removeConfirm", {name: domain.name}), {kind: "warning"})) return;
    setBusy(true);
    try {
      const result = await invoke<{preserved_path: string}>("remove_finder_drive", {domainId: domain.id});
      setMessage(result.preserved_path ? t("finder.preserved", {path: result.preserved_path}) : t("finder.removed"));
      await refresh();
    } catch (e) { setMessage(String(e)); }
    finally { setBusy(false); }
  };
  return <Box sx={{mb: 2}}>
    {message && <Alert severity="info" onClose={() => setMessage("")}>{message}</Alert>}
    {domains.length > 0 && <Typography variant="h6">{t("finder.onDemand")}</Typography>}
    {(domains.length > 0 || message) && <Button disabled={busy} onClick={() => { setMessage(""); refresh(); }}>{t("finder.refresh")}</Button>}
    {domains.map(domain => <Stack key={domain.id} spacing={1} sx={{mb: 2}}>
      <Typography>{domain.name}</Typography>
      <Typography variant="body2" color="text.secondary">{domain.root}</Typography>
      {!domain.user_enabled && <Alert severity="warning">{t("finder.disabled")}</Alert>}
      {domain.disconnected && <Alert severity="warning">{t("finder.disconnected")}</Alert>}
      {!domain.credentials_accessible && <Alert severity="error">{t("finder.keychainUnavailable")} {domain.error_domain} ({domain.error_code})</Alert>}
      {domain.credentials_accessible && !domain.can_reauthorize && <Alert severity="warning">{t("finder.legacyIdentity")}</Alert>}
      {domain.user_enabled && !domain.disconnected && domain.credentials_accessible && <Typography variant="caption" color="text.secondary">{t("finder.registered")}</Typography>}
      <Stack direction="row" spacing={1} useFlexGap sx={{flexWrap: "wrap"}}>
        <Button disabled={busy} onClick={() => open(domain.id)}>{t("finder.open")}</Button>
        <Button disabled={busy || !domain.user_enabled} onClick={() => recover(domain, false)}>{t("finder.retry")}</Button>
        <Button disabled={busy || !domain.server || !domain.can_reauthorize} onClick={() => recover(domain, true)}>{t("settings.reauthorize")}</Button>
        <Button disabled={busy} onClick={() => remove(domain)}>{t("finder.remove")}</Button>
      </Stack>
    </Stack>)}
  </Box>;
}
