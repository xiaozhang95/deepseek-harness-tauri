// dsh Web UI 下载补丁（由 Tauri 壳在页面加载时注入，幂等）。
//
// WebView（WebView2/WKWebView）不像浏览器那样处理程序化的 `<a download>` 点击：
// 没有下载管理器/另存为弹框，点击会被静默丢弃或直接不落盘 —— 这正是打包后
// "Session log 按钮没反应" 的根因。此脚本把导出下载锚点的 click 拦截下来，
// 改走宿主通道：原生另存为对话框 → fetch 归档 → 调 Rust 命令写盘。
// 纯浏览器页面没有 __TAURI_INTERNALS__ 桥，永远不会执行本补丁，行为不变。
//
// 诊断约定：每次拦截把状态写进 window.__dshLast*，Tauri 壳的探测脚本
// （server.rs 轮询）读取并打印到终端；出错时弹原生错误框，用户直接可见。

(() => {
  if (window.__dshNativeSaveInstalled) return
  window.__dshNativeSaveInstalled = true

  const EXPORT_MARK = '/api/session.export'

  async function nativeSave(url, filename) {
    const internals = window.__TAURI_INTERNALS__
    window.__dshLastIntercept = { at: Date.now(), url, filename }
    window.__dshLastError = null
    window.__dshLastSaved = false
    if (!internals || typeof internals.invoke !== 'function') {
      window.__dshLastError = 'no invoke bridge'
      return false
    }
    try {
      // 1) 原生「另存为」对话框（tauri-plugin-dialog），取消返回 null。
      //    注意 save 命令的参数必须包一层 options（JS API 内部是
      //    invoke('plugin:dialog|save', { options })），平铺会报
      //    "missing required key options"
      const path = await internals.invoke('plugin:dialog|save', {
        options: {
          defaultPath: filename,
          filters: [{ name: 'ZIP archive', extensions: ['zip'] }],
        },
      })
      if (!path) {
        window.__dshLastError = 'cancelled'
        return false
      }
      // 2) 取回导出归档（同源 GET；二进制经 base64 过桥，低层 invoke 不带
      //    Uint8Array 二进制通道，且 vendored 前端没有 @tauri-apps/api）
      const response = await fetch(url)
      if (!response.ok) throw new Error('HTTP ' + response.status)
      const blob = await response.blob()
      const data = await new Promise((resolve, reject) => {
        const reader = new FileReader()
        reader.onload = () => resolve(String(reader.result).split(',')[1] ?? '')
        reader.onerror = () => reject(reader.error ?? new Error('blob read failed'))
        reader.readAsDataURL(blob)
      })
      // 3) 宿主写盘
      await internals.invoke('save_session_log', { path, data })
      window.__dshLastSaved = true
      return true
    } catch (error) {
      window.__dshLastError = String(error && error.message ? error.message : error)
      console.error('[dsh-tauri] session log save failed:', window.__dshLastError)
      // 原生错误框：cargo run 默认不开 devtools，必须让用户直接看到错误
      try {
        await internals.invoke('plugin:dialog|message', {
          title: '导出保存失败',
          message: window.__dshLastError,
          kind: 'error',
        })
      } catch { /* 弹框失败忽略 */ }
      return false
    }
  }

  const originalClick = HTMLAnchorElement.prototype.click
  HTMLAnchorElement.prototype.click = function () {
    const href = typeof this.href === 'string' ? this.href : ''
    // 只拦截带 download 属性且指向导出端点的锚点；桥接缺失时回退原始行为
    //（纯浏览器/能力未生效 = 保持原状，不引入更糟的回归）
    if (this.download && href.includes(EXPORT_MARK)) {
      const internals = window.__TAURI_INTERNALS__
      if (internals && typeof internals.invoke === 'function') {
        nativeSave(href, this.download)
        return
      }
    }
    return originalClick.apply(this, arguments)
  }
})()
