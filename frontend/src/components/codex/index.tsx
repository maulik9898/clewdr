import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import TabNavigation from "../common/TabNavigation";
import LoginFlow from "./LoginFlow";
import CredentialVisualization from "./CredentialVisualization";

const CodexTabs: React.FC = () => {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<"login" | "credentials">("login");
  const [refreshKey, setRefreshKey] = useState(0);

  const tabs = [
    { id: "login", label: t("codex.tabs.login"), color: "orange" },
    {
      id: "credentials",
      label: t("codex.tabs.credentials"),
      color: "amber",
    },
  ];

  const handleLoginComplete = () => {
    setRefreshKey((k) => k + 1);
    setActiveTab("credentials");
  };

  return (
    <div className="w-full">
      <TabNavigation
        tabs={tabs}
        activeTab={activeTab}
        onTabChange={(tabId) =>
          setActiveTab(tabId as "login" | "credentials")
        }
        className="mb-6"
      />

      {activeTab === "login" ? (
        <LoginFlow onLoginComplete={handleLoginComplete} />
      ) : (
        <CredentialVisualization key={refreshKey} />
      )}
    </div>
  );
};

export default CodexTabs;
