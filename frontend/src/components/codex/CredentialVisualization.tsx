import React, { useState, useEffect, useCallback } from "react";
import { useTranslation } from "react-i18next";
import Button from "../common/Button";
import StatusMessage from "../common/StatusMessage";
import { getCodexCredentials, deleteCodexCredential } from "../../api";

interface CodexUsageWindow {
  used_percent?: number | null;
  window_minutes?: number | null;
  resets_at?: number | null;
}

interface CodexUsage {
  plan_type?: string | null;
  limit_reached?: boolean | null;
  primary?: CodexUsageWindow | null;
  secondary?: CodexUsageWindow | null;
}

interface CodexCredential {
  label?: string;
  account_id?: string;
  token?: {
    access_token: string;
    refresh_token: string;
    id_token?: string;
    last_refresh: number;
  };
  reset_time?: number | null;
  usage?: CodexUsage | null;
}

interface CredentialData {
  valid: CodexCredential[];
  exhausted: CodexCredential[];
}

const CredentialVisualization: React.FC = () => {
  const { t } = useTranslation();
  const [data, setData] = useState<CredentialData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");

  const fetchCredentials = useCallback(async () => {
    try {
      setLoading(true);
      const result = await getCodexCredentials();
      setData(result);
      setError("");
    } catch (err) {
      setError(
        err instanceof Error ? err.message : t("codex.credentials.fetchError")
      );
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    fetchCredentials();
  }, [fetchCredentials]);

  const handleDelete = async (cred: CodexCredential) => {
    if (!window.confirm(t("codex.credentials.deleteConfirm"))) return;
    try {
      await deleteCodexCredential(cred);
      fetchCredentials();
    } catch (err) {
      setError(
        err instanceof Error ? err.message : t("codex.credentials.deleteError")
      );
    }
  };

  const formatDate = (timestamp: number) => {
    return new Date(timestamp * 1000).toLocaleString();
  };

  const formatWindow = (minutes?: number | null) => {
    if (!minutes || minutes <= 0) return "";
    if (minutes % (60 * 24) === 0) return `${minutes / (60 * 24)}d`;
    if (minutes % 60 === 0) return `${minutes / 60}h`;
    return `${minutes}m`;
  };

  const renderUsageWindow = (
    fallbackLabel: string,
    w?: CodexUsageWindow | null
  ) => {
    if (!w || w.used_percent == null) return null;
    const pct = Math.round(w.used_percent);
    const barColor =
      pct >= 90 ? "bg-red-500" : pct >= 70 ? "bg-yellow-500" : "bg-green-500";
    return (
      <div className="flex items-center gap-2">
        <span className="text-xs text-gray-400 w-8">
          {formatWindow(w.window_minutes) || fallbackLabel}
        </span>
        <div className="h-1.5 w-28 bg-gray-700 rounded-full overflow-hidden">
          <div
            className={`h-full ${barColor}`}
            style={{ width: `${Math.min(pct, 100)}%` }}
          />
        </div>
        <span className="text-xs text-gray-400 w-9">{pct}%</span>
        {w.resets_at != null && (
          <span className="text-xs text-gray-500">
            {t("codex.credentials.resetsAt")}: {formatDate(w.resets_at)}
          </span>
        )}
      </div>
    );
  };

  const getTokenStatus = (cred: CodexCredential) => {
    if (!cred.token) return { text: t("codex.credentials.noToken"), color: "text-red-400" };
    const lastRefresh = cred.token.last_refresh;
    const eightDaysAgo = Date.now() / 1000 - 8 * 24 * 60 * 60;
    if (lastRefresh < eightDaysAgo) {
      return { text: t("codex.credentials.stale"), color: "text-yellow-400" };
    }
    return { text: t("codex.credentials.valid"), color: "text-green-400" };
  };

  if (loading) {
    return (
      <div className="text-center text-gray-400 py-4">
        {t("common.loading")}
      </div>
    );
  }

  if (error) {
    return <StatusMessage type="error" message={error} />;
  }

  const allCreds = [
    ...(data?.valid || []).map((c) => ({ ...c, _status: "valid" as const })),
    ...(data?.exhausted || []).map((c) => ({
      ...c,
      _status: "exhausted" as const,
    })),
  ];

  return (
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <h3 className="text-lg font-medium">
          {t("codex.credentials.title")}
        </h3>
        <Button onClick={fetchCredentials} variant="secondary" className="py-1 px-3 text-sm">
          {t("cookieStatus.refresh")}
        </Button>
      </div>

      {allCreds.length === 0 ? (
        <p className="text-gray-500 text-sm text-center py-4">
          {t("codex.credentials.none")}
        </p>
      ) : (
        <div className="space-y-2">
          {allCreds.map((cred, idx) => {
            const status = getTokenStatus(cred);
            return (
              <div
                key={idx}
                className="bg-gray-800 rounded-lg p-3 flex items-center justify-between"
              >
                <div className="flex-1 min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="text-sm font-medium text-white truncate">
                      {cred.label || cred.account_id || t("codex.credentials.unknown")}
                    </span>
                    <span
                      className={`text-xs px-2 py-0.5 rounded-full ${
                        cred._status === "valid"
                          ? "bg-green-900/50 text-green-400"
                          : "bg-yellow-900/50 text-yellow-400"
                      }`}
                    >
                      {cred._status === "valid"
                        ? t("codex.credentials.active")
                        : t("codex.credentials.cooldown")}
                    </span>
                    {cred.usage?.plan_type && (
                      <span className="text-xs px-2 py-0.5 rounded-full bg-blue-900/50 text-blue-300 uppercase">
                        {cred.usage.plan_type}
                      </span>
                    )}
                    {cred.usage?.limit_reached && (
                      <span className="text-xs px-2 py-0.5 rounded-full bg-red-900/50 text-red-400">
                        {t("codex.credentials.limitReached")}
                      </span>
                    )}
                  </div>
                  <div className="flex items-center gap-3 mt-1">
                    <span className={`text-xs ${status.color}`}>
                      {status.text}
                    </span>
                    {cred.token && (
                      <span className="text-xs text-gray-500">
                        {t("codex.credentials.lastRefresh")}: {formatDate(cred.token.last_refresh)}
                      </span>
                    )}
                    {cred.reset_time && (
                      <span className="text-xs text-yellow-500">
                        {t("codex.credentials.resetsAt")}: {formatDate(cred.reset_time)}
                      </span>
                    )}
                  </div>
                  {cred.usage &&
                    (cred.usage.primary || cred.usage.secondary) && (
                      <div className="mt-2 space-y-1">
                        {renderUsageWindow("5h", cred.usage.primary)}
                        {renderUsageWindow("7d", cred.usage.secondary)}
                      </div>
                    )}
                </div>
                <button
                  onClick={() => handleDelete(cred)}
                  className="ml-2 text-red-400 hover:text-red-300 p-1"
                  title={t("codex.credentials.delete")}
                >
                  <svg
                    className="w-4 h-4"
                    fill="none"
                    stroke="currentColor"
                    viewBox="0 0 24 24"
                  >
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16"
                    />
                  </svg>
                </button>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
};

export default CredentialVisualization;
