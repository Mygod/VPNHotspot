@echo off
REM VPNHotspot Windows构建脚本
REM 提供Windows环境的Java配置指导

echo VPNHotspot Windows Build Script
echo =================================

REM 检查系统
echo 当前系统: Windows

REM 检查Java环境
java -version >nul 2>&1
if %errorlevel% neq 0 (
    echo ❌ Java未安装，请安装Java 17
    echo    下载地址: https://adoptium.net/
    echo    选择: OpenJDK 17 (LTS)
    pause
    exit /b 1
)

echo ✅ Java已安装
java -version 2>&1 | findstr "version" >nul

REM 检查JAVA_HOME
echo 检查JAVA_HOME环境变量...
if "%JAVA_HOME%"=="" (
    echo ⚠️  建议设置JAVA_HOME环境变量
    echo    方法: 系统属性 -> 高级 -> 环境变量
    echo    值: 你的Java 17安装路径
) else (
    echo ✅ JAVA_HOME: %JAVA_HOME%
)

REM 执行构建
echo.
echo 开始构建VPNHotspot...
gradlew.bat clean build

if %errorlevel% neq 0 (
    echo ❌ 构建失败，请检查错误信息
    pause
    exit /b 1
)

echo.
echo ✅ 构建完成！
echo 📦 APK文件位置: mobile\build\outputs\apk\
pause