import React, { useState, useRef, useEffect } from "react";
import { useTranslation } from "react-i18next";
import Button from "../common/Button";
import StatusMessage from "../common/StatusMessage";
import { startCodexLogin, pollCodexLogin } from "../../api";

interface LoginFlowProps {
  onLoginComplete?: () => void;
}

const LoginFlow: React.FC<LoginFlowProps> = ({ onLoginComplete }) => {
  const { t } = useTranslation();
  const [status, setStatus] = useState<
    "idle" | "waiting" | "polling" | "success" | "error"
  >("idle");
  const [verificationUrl, setVerificationUrl] = useState("");
  const [userCode, setUserCode] = useState("");
  const [error, setError] = useState("");
  const [label, setLabel] = useState("");
  const abortRef = useRef<AbortController | null>(null);
  const pollTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    return () => {
      abortRef.current?.abort();
      if (pollTimerRef.current) clearInterval(pollTimerRef.current);
    };
  }, []);

  const handleStartLogin = async () => {
    const controller = new AbortController();
    abortRef.current = controller;

    try {
      setStatus("waiting");
      setError("");
      const data = await startCodexLogin(controller.signal);
      if (controller.signal.aborted) return;

      setVerificationUrl(data.verification_url);
      setUserCode(data.user_code);
      setStatus("polling");

      // Poll at the server-specified interval
      const interval = (data.interval || 5) * 1000;
      const pollFn = async () => {
        if (controller.signal.aborted) {
          if (pollTimerRef.current) clearInterval(pollTimerRef.current);
          return;
        }
        try {
          const result = await pollCodexLogin(
            data.device_auth_id,
            data.user_code,
            data.interval,
            controller.signal
          );
          if (controller.signal.aborted) return;

          if (result.status === "complete") {
            if (pollTimerRef.current) clearInterval(pollTimerRef.current);
            setStatus("success");
            setLabel(result.label || "");
            onLoginComplete?.();
          }
          // "pending" → keep polling
        } catch (err) {
          if (controller.signal.aborted) return;
          if (pollTimerRef.current) clearInterval(pollTimerRef.current);
          setStatus("error");
          setError(
            err instanceof Error ? err.message : t("codex.login.unknownError")
          );
        }
      };

      pollTimerRef.current = setInterval(pollFn, interval);
    } catch (err) {
      if (controller.signal.aborted) return;
      setStatus("error");
      setError(
        err instanceof Error ? err.message : t("codex.login.unknownError")
      );
    }
  };

  const handleCopyCode = () => {
    navigator.clipboard.writeText(userCode);
  };

  return (
    <div className="space-y-4">
      <h3 className="text-lg font-medium">{t("codex.login.title")}</h3>
      <p className="text-sm text-gray-400">{t("codex.login.description")}</p>

      {status === "idle" && (
        <Button onClick={handleStartLogin} className="w-full">
          {t("codex.login.startButton")}
        </Button>
      )}

      {(status === "waiting" || status === "polling") && (
        <div className="space-y-3">
          {userCode && (
            <div className="bg-gray-800 rounded-lg p-4 text-center space-y-2">
              <p className="text-sm text-gray-400">
                {t("codex.login.visitUrl")}
              </p>
              <a
                href={verificationUrl}
                target="_blank"
                rel="noopener noreferrer"
                className="text-blue-400 hover:text-blue-300 underline text-sm break-all"
              >
                {verificationUrl}
              </a>
              <p className="text-sm text-gray-400 mt-2">
                {t("codex.login.enterCode")}
              </p>
              <div className="flex items-center justify-center gap-2">
                <code className="text-2xl font-mono font-bold text-white tracking-widest">
                  {userCode}
                </code>
                <button
                  onClick={handleCopyCode}
                  className="text-gray-400 hover:text-white p-1"
                  title={t("common.copy")}
                >
                  <svg
                    className="w-5 h-5"
                    fill="none"
                    stroke="currentColor"
                    viewBox="0 0 24 24"
                  >
                    <path
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      strokeWidth={2}
                      d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z"
                    />
                  </svg>
                </button>
              </div>
              <p className="text-xs text-gray-500 mt-2">
                {t("codex.login.waiting")}
              </p>
            </div>
          )}
          {status === "waiting" && !userCode && (
            <div className="text-center text-gray-400">
              <div className="animate-spin inline-block w-6 h-6 border-2 border-current border-t-transparent rounded-full mb-2" />
              <p>{t("codex.login.requesting")}</p>
            </div>
          )}
        </div>
      )}

      {status === "success" && (
        <div className="space-y-2">
          <StatusMessage
            type="success"
            message={t("codex.login.success", { label })}
          />
          <Button
            onClick={() => {
              setStatus("idle");
              setUserCode("");
              setVerificationUrl("");
            }}
            variant="secondary"
            className="w-full"
          >
            {t("codex.login.loginAnother")}
          </Button>
        </div>
      )}

      {status === "error" && (
        <div className="space-y-2">
          <StatusMessage type="error" message={error} />
          <Button
            onClick={() => {
              setStatus("idle");
              setError("");
            }}
            variant="secondary"
            className="w-full"
          >
            {t("codex.login.retry")}
          </Button>
        </div>
      )}
    </div>
  );
};

export default LoginFlow;
