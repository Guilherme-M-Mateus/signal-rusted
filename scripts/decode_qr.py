"""Decode the QR code in a snapshot PNG, as a phone camera would. Usage: decode_qr.py FILE"""
import sys
import cv2

img = cv2.imread(sys.argv[1])
data, points, _ = cv2.QRCodeDetector().detectAndDecode(img)
print(data if data else "NO QR DECODED")
sys.exit(0 if data else 1)
