# WebServer修复手动测试指南

## 概述
本指南提供了对VPNHotspot WebServer修复的手动测试步骤，以补充自动化测试。

## 测试环境要求
- Android设备或模拟器
- 已编译的VPNHotspot APK
- 网络连接
- 浏览器应用

## 测试场景

### 1. 剪贴板复制功能测试

#### 测试步骤：
1. 打开VPNHotspot应用
2. 进入设置页面
3. 找到"Web服务器设置"部分
4. 点击"API Key管理"
5. 选择"复制后台地址"

#### 预期结果：
- ✅ 应显示"Web后台地址已复制到剪贴板"的提示
- ✅ 剪贴板中应包含完整的URL（如：http://192.168.1.100:8080/your_api_key）
- ✅ 如果无法获取IP地址，应显示"无法获取IP地址，已复制API Key"并复制API Key

#### 错误场景测试：
1. 在没有网络连接的情况下测试
2. 在剪贴板权限被拒绝的情况下测试

#### 预期错误处理：
- ✅ 应显示适当的错误消息
- ✅ 应用不应崩溃
- ✅ 应有回退行为（复制API Key而不是完整URL）

### 2. WebServer生命周期测试

#### 测试步骤：
1. 启动VPNHotspot应用
2. 检查WebServer是否自动启动
3. 使用浏览器访问WebServer地址
4. 关闭应用
5. 重新打开应用
6. 再次访问WebServer地址

#### 预期结果：
- ✅ 应用启动时WebServer应自动启动
- ✅ 浏览器应能成功访问WebServer
- ✅ 应用关闭时WebServer应停止
- ✅ 重新打开应用时WebServer应重新启动
- ✅ 不应有资源泄漏或端口占用问题

### 3. 端口冲突处理测试

#### 测试步骤：
1. 在设置中将WebServer端口设置为8080
2. 使用其他应用占用8080端口
3. 启动VPNHotspot
4. 检查WebServer是否启动在备用端口

#### 预期结果：
- ✅ WebServer应自动尝试备用端口（9999、10000、10001）
- ✅ 应显示相应的日志消息
- ✅ 设置中的端口应更新为实际使用的端口
- ✅ WebServer应正常工作

### 4. 长时间运行测试

#### 测试步骤：
1. 启动VPNHotspot并确保WebServer运行
2. 让应用在后台运行24小时
3. 定期访问WebServer（每小时一次）
4. 检查内存使用情况
5. 检查WebServer响应性能

#### 预期结果：
- ✅ WebServer应持续可访问
- ✅ 内存使用应保持稳定
- ✅ 响应时间应保持正常
- ✅ 不应有内存泄漏

### 5. 应用重启场景测试

#### 测试步骤：
1. 启动应用并确保WebServer运行
2. 强制关闭应用（通过系统设置或任务管理器）
3. 重新启动应用
4. 检查WebServer状态
5. 访问WebServer

#### 预期结果：
- ✅ 应用重启后WebServer应正常启动
- ✅ 不应有端口占用冲突
- ✅ 所有功能应正常工作

### 6. 网络变化测试

#### 测试步骤：
1. 在WiFi网络下启动应用
2. 记录WebServer的IP地址
3. 切换到移动数据网络
4. 检查WebServer是否仍然可访问
5. 切换回WiFi网络

#### 预期结果：
- ✅ 网络变化时WebServer应适应新的IP地址
- ✅ 剪贴板复制功能应反映新的IP地址
- ✅ 不应有连接问题

## 性能测试

### 并发连接测试
1. 使用多个浏览器标签页同时访问WebServer
2. 检查响应时间和稳定性

### 大文件传输测试
1. 通过WebServer上传/下载大文件
2. 检查内存使用和性能

## 错误恢复测试

### 异常情况处理
1. 在WebServer运行时断开网络连接
2. 在WebServer运行时更改网络设置
3. 在WebServer运行时强制终止相关进程

### 预期恢复行为
- ✅ 应用应能优雅地处理网络异常
- ✅ WebServer应能在网络恢复后重新工作
- ✅ 不应有未处理的异常或崩溃

## 日志检查

### 关键日志消息
在测试过程中，检查以下日志消息：

```
WebServerManager: Starting WebServer on port 8080
WebServerManager: WebServer started successfully on preferred port 8080
WebServerManager: Port 8080 is already in use, trying next port
WebServerManager: WebServer started on fallback port 9999
OkHttpWebServer: WebServer started and listening on port 8080
OkHttpWebServer: Stopping WebServer...
OkHttpWebServer: WebServer stopped successfully
SettingsPreferenceFragment: Successfully copied web backend URL to clipboard
SettingsPreferenceFragment: Unable to get device IP address, falling back to API Key copy
```

## 测试报告模板

### 测试结果记录
对于每个测试场景，记录：
- [ ] 测试通过
- [ ] 测试失败
- [ ] 发现的问题
- [ ] 建议的改进

### 问题报告格式
```
问题描述：
重现步骤：
预期结果：
实际结果：
设备信息：
应用版本：
```

## 自动化测试补充

本手动测试指南补充了以下自动化测试：
- 代码编译测试
- 静态代码分析
- 单元测试覆盖
- 集成测试验证

## 测试完成标准

所有测试场景都应：
- ✅ 功能正常工作
- ✅ 错误处理适当
- ✅ 性能表现良好
- ✅ 用户体验友好
- ✅ 日志记录完整

---

**注意**: 在进行手动测试时，请确保有足够的时间进行长时间运行测试，并在不同的网络环境和设备配置下进行测试。