#!/bin/bash
# Integration test: S3 service (incl. multipart round-trip).
# Mirrors test_webdav_integration.sh. Runs against the running MultiFS S3
# gateway on port 9000. All temp artifacts live under /tmp/inttest-s3-$$ and
# are removed on exit (trap) — see cleanup at the bottom.
#
# The multipart portion exercises the server-side fix: UploadPart now reads +
# stores the part body and CompleteMultipartUpload assembles it, so a GET must
# return exactly the uploaded parts (previously parts were discarded).
set -e

S3="http://100.100.30.59:9000"
TMP="/tmp/inttest-s3-$$"
PASS=0
FAIL=0

check() {
    local name="$1"
    if [ "$2" = "0" ]; then echo "  ✅ $name"; PASS=$((PASS+1)); else echo "  ❌ $name"; FAIL=$((FAIL+1)); fi
}
check_code() {
    local name="$1" actual="$2" expected="$3"
    if [ "$actual" = "$expected" ]; then check "$name" 0; else echo "     expected HTTP $expected, got $actual"; check "$name" 1; fi
}

# Ensure tmp dir is cleaned up even on failure / early exit.
cleanup() {
    rm -rf "$TMP"
    # Also drop any leftover test objects we may have created in the bucket.
    curl -s -o /dev/null -X DELETE "$S3/$BUCKET/final.bin" 2>/dev/null || true
    curl -s -o /dev/null -X DELETE "$S3/$BUCKET" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Integration Test: S3 (incl. multipart) ==="
mkdir -p "$TMP"

BUCKET="inttest-s3-$$"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET")
check_code "Create bucket $BUCKET" "$CODE" "200"

# ---- Generate a test file (5 MB, over the 32 MB chunk threshold only matters
# ---- for PUT; we use a small file for S3 multipart so it buffers in memory). 
python3 - "$TMP/final.bin" <<'PY'
import sys
n = 1024*1024
data = bytes(((i*13+7)%256) for i in range(n))
open(sys.argv[1],'wb').write(data)
PY
HASH=$(sha256sum "$TMP/final.bin" | awk '{print $1}')
echo "  Test file: $(stat -c%s "$TMP/final.bin") bytes, SHA256: $HASH"

# =============================
# A. S3 multipart round-trip
# =============================
echo "--- A. Multipart upload ---"

# A1. Initiate
INIT_XML=$(curl -s -X POST "$S3/$BUCKET/final.bin?uploads")
UPLOAD_ID=$(echo "$INIT_XML" | sed -n 's:.*<UploadId>\(.*\)</UploadId>.*:\1:p')
if [ -n "$UPLOAD_ID" ]; then check "Initiate multipart returns UploadId" 0; else echo "$INIT_XML"; check "Initiate multipart returns UploadId" 1; fi

# A2. Upload part 1 (half the file)
SIZE=$(stat -c%s "$TMP/final.bin")
HALF=$((SIZE/2))
head -c "$HALF" "$TMP/final.bin" > "$TMP/part1.bin"
tail -c +$((HALF+1)) "$TMP/final.bin" > "$TMP/part2.bin"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET/final.bin?partNumber=1&uploadId=$UPLOAD_ID" --data-binary @"$TMP/part1.bin")
check_code "Upload part 1" "$CODE" "200"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET/final.bin?partNumber=2&uploadId=$UPLOAD_ID" --data-binary @"$TMP/part2.bin")
check_code "Upload part 2" "$CODE" "200"

# A3. Complete
COMP_XML=$(curl -s -X POST "$S3/$BUCKET/final.bin?uploadId=$UPLOAD_ID")
echo "$COMP_XML" | grep -q "CompleteMultipartUploadResult" && check "Complete multipart returns result XML" 0 || { echo "$COMP_XML"; check "Complete multipart returns result XML" 1; }

# A4. Download and verify bytes == parts in order
curl -s "$S3/$BUCKET/final.bin" -o "$TMP/final-dl.bin"
DL_HASH=$(sha256sum "$TMP/final-dl.bin" | awk '{print $1}')
if [ "$DL_HASH" = "$HASH" ]; then check "Multipart round-trip SHA256 match" 0; else check "Multipart round-trip SHA256 match (got $DL_HASH)" 1; fi
cmp -s "$TMP/final.bin" "$TMP/final-dl.bin" && check "Multipart content exact match" 0 || check "Multipart content exact match" 1
DL_SIZE=$(stat -c%s "$TMP/final-dl.bin"); EXPECTED=$SIZE
[ "$DL_SIZE" = "$EXPECTED" ] && check "Multipart size: $EXPECTED" 0 || check "Multipart size: expected $EXPECTED got $DL_SIZE" 1

# =============================
# B. S3 single PUT/GET round-trip
# =============================
echo "--- B. Single PUT/GET ---"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET/simple.txt" --data-binary "hello s3 integration")
check_code "Simple PUT" "$CODE" "200"
CT=$(curl -s "$S3/$BUCKET/simple.txt")
[ "$CT" = "hello s3 integration" ] && check "Simple GET content" 0 || check "Simple GET content" 1

# =============================
# C. HEAD + Delete object + cleanup
# =============================
echo "--- C. HEAD / Delete ---"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -I "$S3/$BUCKET/simple.txt")
check_code "HEAD object (exists)" "$CODE" "200"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$S3/$BUCKET/simple.txt")
check_code "Delete object" "$CODE" "204"

# =============================
# D. Cleanup bucket (also done by trap)
# =============================
echo "--- Clean up ---"
curl -s -o /dev/null -X DELETE "$S3/$BUCKET/final.bin"
curl -s -o /dev/null -X DELETE "$S3/$BUCKET"
check "Removed test bucket/objects" 0

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] && echo "✅ All S3 integration tests passed!" || echo "❌ Some S3 integration tests failed"
echo "Temp dir cleaned: $TMP"
exit $FAIL
