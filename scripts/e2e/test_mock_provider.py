#!/usr/bin/env python3
"""Run protocol regressions without any agent, credentials or upstream traffic."""
import base64
import hashlib
import http.client
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import unittest


class MockProtocols(unittest.TestCase):
    def test_http_sse_websocket_hashes_and_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            server = subprocess.Popen([sys.executable, str(Path(__file__).with_name('mock_provider.py')), '--port-file', str(root/'port'), '--log', str(root/'requests.jsonl'), '--chunk-delay-ms', '20'])
            try:
                deadline = time.monotonic() + 5
                while not (root/'port').exists():
                    self.assertIsNone(server.poll())
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.01)
                port = int((root/'port').read_text())
                headers = {'Authorization': 'Bearer mock-sensitive-value', 'X-Api-Key': 'mock-sensitive-key', 'Content-Type': 'application/json', 'ChatGPT-Account-ID': 'mock-account', 'OpenAI-Beta': 'responses_websockets=2026-02-06'}

                def post(path, body):
                    raw = json.dumps(body, separators=(',', ':')).encode()
                    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                    connection.request('POST', path, raw, headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    content = response.read().decode()
                    connection.close()
                    return content, raw

                content, raw = post('/v1/messages', {'model':'mock-model','stream':True,'messages':[{'role':'user','content':'hello'}]})
                self.assertIn('event: message_start', content)
                self.assertIn('"output_tokens": 7', content)
                request = json.loads((root/'requests.jsonl').read_text().splitlines()[-1])
                self.assertEqual(request['body_sha256'], hashlib.sha256(raw).hexdigest())
                self.assertEqual(request['headers']['Authorization'], '[REDACTED]')
                self.assertEqual(request['header_value_sha256']['authorization'], [hashlib.sha256(headers['Authorization'].encode()).hexdigest()])
                self.assertEqual(request['header_value_sha256']['openai-beta'], [hashlib.sha256(headers['OpenAI-Beta'].encode()).hexdigest()])
                chunks = [json.loads(line) for line in (root/'requests.events.jsonl').read_text().splitlines()]
                self.assertGreaterEqual(chunks[-1]['sent_ns'] - chunks[0]['sent_ns'], 80_000_000)

                content, _ = post('/v1/chat/completions', {'stream':True,'messages':[{'role':'user','content':'RUN_TOOL'}], 'tools':[{'type':'function','function':{'name':'bash','parameters':{'properties':{'command':{'type':'string'},'description':{'type':'string'}}}}}]})
                events = [json.loads(line[6:]) for line in content.splitlines() if line.startswith('data: ') and line != 'data: [DONE]']
                arguments = json.loads(events[0]['choices'][0]['delta']['tool_calls'][0]['function']['arguments'])
                self.assertEqual(arguments, {'command':'echo hra-e2e','description':'Harmless mock E2E echo'})
                self.assertEqual(events[-1]['usage']['completion_tokens'], 7)

                grok_shell = {'type':'function','name':'run_terminal_command','parameters':{'properties':{'command':{'type':'string'},'description':{'type':'string'}}}}
                title = {'type':'function','name':'session_title','parameters':{'properties':{'title':{'type':'string'}}}}
                content, _ = post('/v1/responses', {'model':'mock-model','input':[{'role':'user','content':'RUN_TOOL'}], 'tools':[grok_shell,title], 'tool_choice':{'type':'function','name':'session_title'}})
                self.assertEqual(json.loads(content)['output'][0]['type'], 'message')
                content, _ = post('/v1/responses', {'model':'mock-model','input':[{'role':'user','content':'RUN_TOOL'}], 'tools':[grok_shell]})
                item = json.loads(content)['output'][0]
                self.assertEqual(item['name'],'run_terminal_command')
                self.assertEqual(json.loads(item['arguments']),{'command':'echo hra-e2e','description':'Harmless mock E2E echo'})
                content, _ = post('/v1/responses', {'model':'mock-model','input':[{'role':'user','content':'RUN_TOOL'}], 'tools':[title]})
                self.assertEqual(json.loads(content)['output'][0]['type'],'message')
                content, _ = post('/v1/responses', {'model':'mock-model','stream':True,'generate':False,'input':'hello'})
                events = [json.loads(line[6:]) for line in content.splitlines() if line.startswith('data: ')]
                self.assertEqual([e['type'] for e in events], ['response.created','response.completed'])
                self.assertEqual(events[-1]['response']['output'], [])
                self.assertIsNone(events[-1]['response']['usage'])

                content, _ = post('/v1beta/models/mock-model:streamGenerateContent?alt=sse&key=mock-sensitive-query', {'contents':[{'role':'user','parts':[{'text':'hello'}]}]})
                events = [json.loads(line[6:]) for line in content.splitlines() if line.startswith('data: ')]
                self.assertEqual(len(events), 3)
                self.assertEqual(''.join(p['text'] for e in events for c in e['candidates'] for p in c['content']['parts']), 'hra-e2e mock response')
                self.assertEqual(events[-1]['usageMetadata']['cachedContentTokenCount'], 3)
                self.assertEqual(len({e['responseId'] for e in events}), 1)
                self.assertNotIn('mock-sensitive-query', (root/'requests.jsonl').read_text())

                connection = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                connection.connect()
                connection.request('GET', '/v1/responses', headers=dict(headers, Upgrade='websocket', Connection='Upgrade', **{'Sec-WebSocket-Key':base64.b64encode(b'0123456789abcdef').decode(),'Sec-WebSocket-Version':'13'}))
                response = connection.getresponse()
                self.assertEqual(response.status, 101)
                sock = connection.sock
                self.assertIsNotNone(sock)
                stream = sock.makefile('rb')

                def send(opcode, data, final=True):
                    mask = b'abcd'
                    size = len(data)
                    header = bytes([(0x80 if final else 0) | opcode])
                    header += bytes([0x80 | size]) if size < 126 else b'\xfe' + struct.pack('!H', size)
                    sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i,b in enumerate(data)))

                def receive():
                    first, second = stream.read(2)
                    size = second & 127
                    if size == 126: size = struct.unpack('!H', stream.read(2))[0]
                    elif size == 127: size = struct.unpack('!Q', stream.read(8))[0]
                    self.assertFalse(second & 0x80)
                    return first & 15, stream.read(size)

                send(9, b'ping')
                self.assertEqual(receive(), (10,b'ping'))
                warmup = json.dumps({'type':'response.create','generate':False,'model':'mock-model','input':[]}).encode()
                send(1,warmup[:20],False)
                send(0,warmup[20:])
                self.assertEqual(json.loads(receive()[1])['type'], 'response.created')
                self.assertEqual(json.loads(receive()[1])['response']['output'], [])
                tool = {'type':'response.create','model':'mock-model','input':[{'role':'user','content':'RUN_TOOL'}], 'tools':[{'type':'function','name':'exec_command','parameters':{'properties':{'cmd':{'type':'string'},'login':{'type':'boolean'}}}}]}
                send(1,json.dumps(tool).encode())
                events=[]
                while not events or events[-1]['type'] != 'response.completed': events.append(json.loads(receive()[1]))
                item = events[-1]['response']['output'][0]
                self.assertEqual(item['name'], 'exec_command')
                self.assertEqual(json.loads(item['arguments']), {'cmd':'echo hra-e2e','login':False})
                tool_result = {'type':'response.create','model':'mock-model','input':[{'type':'function_call_output','call_id':item['call_id'],'output':'hra-e2e'}]}
                send(1,json.dumps(tool_result).encode())
                events=[]
                while not events or events[-1]['type'] != 'response.completed': events.append(json.loads(receive()[1]))
                self.assertEqual(events[-1]['response']['output'][0]['content'][0]['text'], 'hra-e2e mock response')
                self.assertEqual(events[-1]['response']['usage']['input_tokens'], 11)
                send(8,struct.pack('!H',1000))
                self.assertEqual(receive()[0],8)
                stream.close()
                connection.close()
                ws_requests = [json.loads(line) for line in (root/'requests.jsonl').read_text().splitlines() if json.loads(line)['transport']=='websocket']
                self.assertEqual(len(ws_requests),3)
                self.assertEqual(ws_requests[0]['body_sha256'],hashlib.sha256(warmup).hexdigest())
            finally:
                server.terminate()
                server.wait(timeout=5)


if __name__ == '__main__':
    unittest.main()
