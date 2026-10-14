# 实现计划

- [x] 1. 修复SettingsPreferenceFragment中的重复剪贴板复制函数调用
  - 移除第248行的重复`copyWebBackendUrlToClipboard(currentApiKey)`调用
  - 验证函数只在用户选择选项时调用一次
  - 测试剪贴板功能以确保其正常工作
  - _需求: 1.1, 1.2, 1.3_

- [x] 2. 为MainActivity添加适当的WebServer生命周期管理
  - 在MainActivity类中实现`onDestroy()`方法
  - 在onDestroy方法中添加`WebServerManager.stop()`调用
  - 在onCreate中为WebServer启动失败添加错误处理
  - 为WebServer生命周期事件添加日志记录
  - _需求: 2.1, 2.2, 2.3, 2.6_

- [x] 3. 增强WebServerManager资源清理和错误处理
  - 改进`stop()`方法以确保完整的资源清理
  - 添加端口冲突检测和解决逻辑
  - 实现备用端口的重试机制（9999、10000、10001）
  - 添加全面的异常处理和日志记录
  - _需求: 2.4, 2.5, 3.4, 3.5_

- [x] 4. 改进OkHttpWebServer资源管理和清理
  - 在handleConnection方法中使用适当的try-finally块增强套接字清理
  - 在stop()方法中改进带超时的线程池关闭
  - 在stop()方法中添加全面的资源清理
  - 确保协程作用域被正确取消和清理
  - _需求: 3.1, 3.2, 3.3, 3.4_

- [x] 5. 为剪贴板操作添加全面的错误处理
  - 为剪贴板访问添加SecurityException的try-catch块
  - 实现无法获取IP地址时的回退行为
  - 为剪贴板操作失败添加用户反馈
  - 确保提示消息正确显示
  - _需求: 1.3, 1.4_

- [x] 6. 测试并验证所有修复都能正常工作
  - 手动测试剪贴板复制功能
  - 测试长时间运行后WebServer的可访问性
  - 测试应用重启场景以确保WebServer正确启动
  - 验证应用终止时的适当资源清理
  - 测试端口冲突场景和解决方案
  - _需求: 2.1, 2.2, 2.3, 2.4, 2.5_