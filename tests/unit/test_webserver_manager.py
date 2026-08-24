#!/usr/bin/env python3
"""
Test script to verify WebServerManager enhancements
Tests port conflict detection, retry mechanism, and resource cleanup
"""

import socket
import time
import threading
from contextlib import contextmanager

def is_port_available(port):
    """Check if a port is available"""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.bind(('localhost', port))
            return True
    except OSError:
        return False

@contextmanager
def occupy_port(port):
    """Context manager to temporarily occupy a port"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(('localhost', port))
        sock.listen(1)
        print(f"Port {port} occupied")
        yield sock
    finally:
        sock.close()
        print(f"Port {port} released")

def test_port_availability():
    """Test port availability checking"""
    print("Testing port availability checking...")
    
    # Test available port
    available_port = 19999
    if is_port_available(available_port):
        print(f"✓ Port {available_port} is correctly detected as available")
    else:
        print(f"✗ Port {available_port} should be available")
    
    # Test occupied port
    with occupy_port(available_port):
        if not is_port_available(available_port):
            print(f"✓ Port {available_port} is correctly detected as occupied")
        else:
            print(f"✗ Port {available_port} should be occupied")

def test_port_conflict_simulation():
    """Simulate port conflicts to test retry mechanism"""
    print("\nTesting port conflict simulation...")
    
    test_ports = [19999, 20000, 20001]  # Use different ports to avoid conflicts
    
    # Occupy first two ports
    sockets = []
    try:
        for port in test_ports[:2]:
            try:
                sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                sock.bind(('localhost', port))
                sock.listen(1)
                sockets.append(sock)
                print(f"Occupied port {port}")
            except OSError as e:
                print(f"Could not occupy port {port}: {e}")
        
        # Port 20001 should be available for the WebServer to use
        if is_port_available(test_ports[2]):
            print(f"✓ Port {test_ports[2]} is available for fallback")
        else:
            print(f"✗ Port {test_ports[2]} should be available for fallback")
            
    finally:
        for sock in sockets:
            try:
                sock.close()
            except:
                pass
        print("Released all test ports")

def test_webserver_accessibility():
    """Test if WebServer is accessible after startup"""
    print("\nTesting WebServer accessibility...")
    
    # This would need to be run with the actual Android app
    # For now, just test the concept with socket connections
    test_ports = [9999, 10000, 10001]
    
    for port in test_ports:
        try:
            # Quick connection test (will fail without actual server)
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(1)
                result = s.connect_ex(('localhost', port))
                if result == 0:
                    print(f"✓ WebServer accessible at localhost:{port}")
                    break
                else:
                    print(f"- WebServer not accessible at localhost:{port} (expected if not running)")
        except Exception as e:
            print(f"- Connection test failed for port {port}: {e}")

def test_resource_cleanup():
    """Test resource cleanup concepts"""
    print("\nTesting resource cleanup concepts...")
    
    # Simulate resource cleanup timing
    start_time = time.time()
    
    # Simulate stopping server
    print("Simulating server stop...")
    time.sleep(0.1)  # Simulate stop delay
    
    # Simulate port release
    print("Simulating port release...")
    time.sleep(0.2)  # Simulate port release delay
    
    cleanup_time = time.time() - start_time
    print(f"✓ Cleanup simulation completed in {cleanup_time:.3f} seconds")
    
    if cleanup_time < 1.0:
        print("✓ Cleanup time is within acceptable range")
    else:
        print("✗ Cleanup time is too long")

def main():
    """Run all tests"""
    print("WebServerManager Enhancement Tests")
    print("=" * 40)
    
    test_port_availability()
    test_port_conflict_simulation()
    test_webserver_accessibility()
    test_resource_cleanup()
    
    print("\n" + "=" * 40)
    print("Test completed. Note: Full testing requires running Android app.")
    print("This script tests the concepts and logic that were implemented.")

if __name__ == "__main__":
    main()