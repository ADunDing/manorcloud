// 複製文字到剪貼簿。
//
// 剪貼簿 API（navigator.clipboard）只在安全環境（HTTPS 或 localhost）可用：
// 用 http://192.168.x.x 連線時不存在。這時改用舊的 document.execCommand("copy")：
// 它在 http 下仍然可以使用，只要是由使用者的點擊觸發的。
//
// 舊方法需要先選取一段文字：建立一個看不見的文字框放進去、選取、複製、移除。
// 對話框（showModal）開著時，對話框以外的元素無法取得焦點，所以文字框要放在同一個對話框裡。

/** 複製成功回傳 true。near：觸發複製的元素（決定暫時的文字框放在哪裡） */
export async function copyText(text: string, near?: Element): Promise<boolean> {
  if (window.isSecureContext && "clipboard" in navigator) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // 使用者拒絕權限等情況：改用舊方法
    }
  }
  const host = near?.closest("dialog") ?? document.body;
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  // 放在畫面外：看不見，也不會讓畫面捲動
  area.style.position = "fixed";
  area.style.top = "-1000px";
  area.style.opacity = "0";
  host.append(area);
  const focused = document.activeElement;
  area.focus();
  area.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  }
  area.remove();
  if (focused instanceof HTMLElement) {
    focused.focus();
  }
  return copied;
}
