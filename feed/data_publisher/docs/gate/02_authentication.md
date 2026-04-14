# Gate.io API v4 Authentication

## API Key Management

- Generate in web console: [Account Management] -> [APIv4 Keys]
- Each account can create up to 20 API keys
- Each key has independent permission configuration
- Keys consist of: **Key** (Access Key) and **Secret Key** (for signature encryption)
- IP whitelist: up to 20 IPv4 addresses per key; if not set, IP validation is skipped
- Modifications to keys take up to 5 minutes to take effect
- TestNet has **separate** API keys (generated in "Futures TestNet APIKeys" tab)

## Permission Groups

| Product | Read-only | Read-write |
|---------|-----------|------------|
| `spot/margin` | Query orders | Query + place orders |
| `perpetual contract` | Query orders | Query + place orders |
| `delivery contract` | Query orders | Query + place orders |
| `wallet` | Query withdrawal/transfer records | Query + fund transfers |
| `withdrawal` | Query cash withdrawal records | Query + withdraw |

Rule: All GET operations = read requests; all others = write requests.

## REST API Authentication

### Required Headers

| Header | Value |
|--------|-------|
| `KEY` | Your API key |
| `Timestamp` | Current Unix time in seconds (max 60 seconds gap from server) |
| `SIGN` | `HexEncode(HMAC_SHA512(secret, signature_string))` |

### Signature String Construction

```
Request Method\n
Request URL\n
Query String\n
HexEncode(SHA512(Request Payload))\n
Timestamp
```

Five components joined by `\n`:

1. **Request Method**: UPPERCASE (e.g., `POST`, `GET`)
2. **Request URL**: Path only, no protocol/host/port (e.g., `/api/v4/futures/orders`)
3. **Query String**: Not URL-encoded, same order as in the URL (e.g., `status=finished&limit=50`). Empty string if no params.
4. **Request Payload Hash**: SHA512 of the request body, hex-encoded. For empty body, use hash of empty string:
   ```
   cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e
   ```
5. **Timestamp**: Same value as the `Timestamp` header

### Python Example

```python
import hashlib
import hmac
import time
import requests

api_key = 'YOUR_API_KEY'
api_secret = 'YOUR_API_SECRET'

def gen_sign(method, url, query_string=None, payload_string=None):
    t = time.time()
    m = hashlib.sha512()
    m.update((payload_string or "").encode('utf-8'))
    hashed_payload = m.hexdigest()

    s = '%s\n%s\n%s\n%s\n%s' % (method, url, query_string or "", hashed_payload, int(t))
    sign = hmac.new(api_secret.encode('utf-8'), s.encode('utf-8'), hashlib.sha512).hexdigest()

    return {'KEY': api_key, 'Timestamp': str(int(t)), 'SIGN': sign}

# Example: GET /api/v4/wallet/total_balance
headers = gen_sign('GET', '/api/v4/wallet/total_balance')
response = requests.get('https://api.gateio.ws/api/v4/wallet/total_balance', headers=headers)
```

### JavaScript (Google Apps Script) Example

```javascript
function gen_sign(key, secret, method, url, query_param = "", payload_string = "") {
  const t = (Date.now() / 1000).toString();
  
  // Step 1: SHA-512 hash of payload
  const hashedPayload = Utilities.computeDigest(
    Utilities.DigestAlgorithm.SHA_512, payload_string, Utilities.Charset.UTF_8
  );
  const hexPayload = hashedPayload.map(b => 
    ("0" + ((b < 0 && b + 256) || b).toString(16)).slice(-2)
  ).join("");
  
  // Step 2: Construct signature string
  const signString = `${method}\n${url}\n${query_param}\n${hexPayload}\n${t}`;
  
  // Step 3: HMAC-SHA512
  const hmacResult = Utilities.computeHmacSignature(
    Utilities.MacAlgorithm.HMAC_SHA_512, signString, secret, Utilities.Charset.UTF_8
  );
  const sign = hmacResult.map(b =>
    ("0" + ((b < 0 && b + 256) || b).toString(16)).slice(-2)
  ).join("");
  
  return { KEY: key, Timestamp: t, SIGN: sign };
}
```

## WebSocket Authentication

WebSocket uses the same HMAC-SHA512 algorithm but with a **different signature string format**.

### Signature String Format (WebSocket)

```
channel=<channel>&event=<event>&time=<time>
```

Where:
- `<channel>`: channel name (e.g., `futures.orders`, `spot.orders`)
- `<event>`: event name (e.g., `subscribe`)
- `<time>`: unix timestamp in seconds

### Auth Object in WebSocket Request

```json
{
  "method": "api_key",
  "KEY": "your_api_key",
  "SIGN": "hmac_sha512_hex_signature"
}
```

### Python WebSocket Auth Example

```python
import hmac
import hashlib
import time
import json

def gen_ws_sign(channel, event, timestamp):
    api_key = 'YOUR_API_KEY'
    api_secret = 'YOUR_API_SECRET'
    
    s = 'channel=%s&event=%s&time=%d' % (channel, event, timestamp)
    sign = hmac.new(
        api_secret.encode('utf-8'),
        s.encode('utf-8'),
        hashlib.sha512
    ).hexdigest()
    
    return {'method': 'api_key', 'KEY': api_key, 'SIGN': sign}

# Example: Subscribe to futures.orders
request = {
    'id': int(time.time() * 1e6),
    'time': int(time.time()),
    'channel': 'futures.orders',
    'event': 'subscribe',
    'payload': ["BTC_USDT"]
}
request['auth'] = gen_ws_sign(
    request['channel'], request['event'], request['time']
)

# Send json.dumps(request) via websocket
```

## WebSocket Account Trade Login

For WebSocket trading API (order placement, cancellation via WebSocket), a separate login step is required.

### Login Request

Channel: `futures.login` (or `spot.login`)

```json
{
  "time": 1234567890,
  "channel": "futures.login",
  "event": "api",
  "payload": {
    "req_id": "unique_request_id",
    "api_key": "your_api_key",
    "timestamp": "1234567890",
    "signature": "hmac_sha512_hex_signature",
    "req_header": {}
  }
}
```

### Login Response

```json
{
  "request_id": "unique_request_id",
  "header": {
    "response_time": "1234567890123",
    "channel": "futures.login",
    "event": "api",
    "client_id": "xxx",
    "conn_id": "connection_id",
    "conn_trace_id": "trace_id",
    "trace_id": "execution_trace_id"
  },
  "data": {
    "result": {
      "api_key": "your_api_key",
      "uid": "user_id"
    }
  }
}
```
