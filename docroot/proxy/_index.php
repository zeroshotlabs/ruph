<?php

// General proxy endpoint.
//
// Add new proxy handlers by extending the switch below. The default handler
// matches the Cloudflare Browser Rendering request shape:
// {
//   "url": "https://example.com",
//   "accountId": "...",
//   "apiToken": "..."
// }

header("Access-Control-Allow-Origin: *");
header("Access-Control-Allow-Methods: POST, OPTIONS");
header("Access-Control-Allow-Headers: Content-Type, Authorization");
header("Content-Type: application/json");

if ($_SERVER['REQUEST_METHOD'] === 'OPTIONS') {
    http_response_code(200);
    exit();
}

if ($_SERVER['REQUEST_METHOD'] !== 'POST') {
    http_response_code(405);
    echo json_encode(['error' => 'Method not allowed']);
    exit();
}

$rawBody = isset($_SERVER['RUPH_RAW_BODY']) ? $_SERVER['RUPH_RAW_BODY'] : file_get_contents('php://input');
$input = json_decode($rawBody, true);

if (!is_array($input)) {
    http_response_code(400);
    echo json_encode(['error' => 'Invalid JSON body']);
    exit();
}

$proxy = isset($input['proxy']) ? $input['proxy'] : 'cloudflareBrowserRendering';

if ($proxy === 'cloudflareBrowserRendering') {
        if (!array_key_exists('url', $input)) {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        if (!array_key_exists('accountId', $input)) {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        if (!array_key_exists('apiToken', $input)) {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        $url = $input['url'];
        $accountId = $input['accountId'];
        $apiToken = $input['apiToken'];

        if ($url === '') {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        if ($accountId === '') {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        if ($apiToken === '') {
            http_response_code(400);
            echo json_encode(['error' => 'Missing required fields: url, accountId, apiToken']);
            exit();
        }

        $viewport = isset($input['viewport']) ? $input['viewport'] : [];
        $width = isset($viewport['width']) ? $viewport['width'] : 1920;
        $height = isset($viewport['height']) ? $viewport['height'] : 1080;
        $format = isset($input['format']) ? $input['format'] : 'png';
        $waitUntil = isset($input['wait_until']) ? $input['wait_until'] : 'networkidle';

        $targetUrl = 'https://api.cloudflare.com/client/v4/accounts/' . rawurlencode($accountId) . '/browser_rendering';
        $requestBody = json_encode([
            'url' => $url,
            'viewport' => ['width' => $width, 'height' => $height],
            'format' => $format,
            'wait_until' => $waitUntil,
        ]);

        if (function_exists('http_request')) {
            echo http_request('POST', $targetUrl, [
                'Authorization' => 'Bearer ' . $apiToken,
                'Content-Type' => 'application/json',
            ], $requestBody);
            exit();
        }

        if (!function_exists('curl_init')) {
            http_response_code(500);
            echo json_encode(['error' => 'No HTTP client available for proxy request']);
            exit();
        }

        $ch = curl_init($targetUrl);
        curl_setopt($ch, CURLOPT_POST, true);
        curl_setopt($ch, CURLOPT_POSTFIELDS, $requestBody);
        curl_setopt($ch, CURLOPT_HTTPHEADER, [
            'Authorization: Bearer ' . $apiToken,
            'Content-Type: application/json',
        ]);
        curl_setopt($ch, CURLOPT_RETURNTRANSFER, true);
        curl_setopt($ch, CURLOPT_TIMEOUT, 30);

        $response = curl_exec($ch);
        $httpCode = curl_getinfo($ch, CURLINFO_HTTP_CODE);
        $curlError = curl_error($ch);
        curl_close($ch);

        http_response_code($httpCode);

        if ($curlError) {
            echo json_encode(['error' => 'Cloudflare API error: ' . $curlError]);
        } else {
            echo $response;
        }
        exit();
}

http_response_code(400);
echo json_encode(['error' => 'Unknown proxy']);
exit();
