const https = require('node:https');
const fs = require('node:fs');
const [leaf, version, chain] = process.argv.slice(1);
const server = https.createServer({
  key: fs.readFileSync(`${leaf}.key`),
  cert: fs.readFileSync(`${leaf}.pem`, 'utf8') + (chain === 'omit' ? '' : fs.readFileSync('intermediate.pem', 'utf8')),
  minVersion: version,
  maxVersion: version,
}, (req, res) => {
  let body = '';
  req.setEncoding('utf8');
  req.on('data', part => { body += part; });
  req.on('end', () => {
    fs.appendFileSync('requests.jsonl', JSON.stringify({
      authorization: req.headers.authorization,
      contentType: req.headers['content-type'],
      stream: req.headers['vl-stream-fields'],
      body,
    }) + '\n');
    if (req.url === '/stall') return;
    if (req.url === '/truncate') { req.socket.destroy(); return; }
    const status = {'/reject': 401, '/busy': 429, '/redirect': 302, '/unavailable': 503}[req.url] || 204;
    res.writeHead(status, { 'Content-Length': '0', Location: 'https://wrong.test/' });
    res.end();
  });
});
server.on('tlsClientError', () => {});
server.listen(0, '127.0.0.1', () => console.log(server.address().port));
