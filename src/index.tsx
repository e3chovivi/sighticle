/* @refresh reload */
import { render } from "solid-js/web";
import App from "./App";
import RecBar from "./RecBar";
import Settings from "./Settings";

// 同一份前端代码按窗口路由：
//   #recbar   → 录制控制条 / 录后编辑小窗口
//   #settings → 设置窗口
//   其余       → 全屏覆盖层（模式选择 / 框选 / 截图标注）
const hash = window.location.hash;
const view = hash === "#recbar" ? <RecBar /> : hash === "#settings" ? <Settings /> : <App />;

render(() => view, document.getElementById("root") as HTMLElement);
