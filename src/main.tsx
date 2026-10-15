import React from "react";
import ReactDOM from "react-dom/client";

import App from "./App";
import "./index.css";

// 监控台使用固定的深色工作区，库存状态和任务进度在长时间运行时保持一致。
document.documentElement.classList.add("dark");

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
