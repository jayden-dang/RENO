import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createAssistantMessageEventStream } from "@earendil-works/pi-ai";

// Scripted-tool mode: a prompt that names a script makes the mock model call tools instead of
// answering. A script is one step per model call of the turn: a list of tool calls, 'hold' (wait
// for the file probe-release-hold, or an abort), or the closing text. Every other prompt behaves
// as before.
const codemodeScript = "await tools.read({path:'a.txt'}); await tools.mcp__probe__ping({message:'nested'}); return 'nested-done';";
const SCRIPTS = {
  'script-tools': [[
    {name:'read', arguments:{path:'a.txt'}},
    {name:'grep', arguments:{pattern:'needle', path:'.', glob:'*.txt'}},
    {name:'find', arguments:{pattern:'*.txt'}},
    {name:'ls', arguments:{path:'.'}},
    {name:'edit', arguments:{path:'edit-me.txt', edits:[{oldText:'before', newText:'after'}]}},
    {name:'write', arguments:{path:'written.txt', content:'written by the script'}},
    {name:'bash', arguments:{command:'echo PROBE_MUTATE > bash-ran.txt'}},
    {name:'mcp__probe__ping', arguments:{message:'direct'}},
    {name:'codemode', arguments:{code:codemodeScript}},
  ], 'SCRIPTED-DONE'],
  'script-200': [
    Array.from({length:200}, (_, n) => ({name:'read', arguments:{path:'a.txt'}})),
    'SCRIPTED-200-DONE',
  ],
  'script-hold': [[{name:'read', arguments:{path:'a.txt'}}], 'hold', 'SCRIPTED-HELD-DONE'],
  'script-plant': [
    [{name:'probe_plant_credential', arguments:{}}],
    [{name:'read', arguments:{path:'a.txt'}}],
    'SCRIPTED-PLANT-DONE',
  ],
};

export default function(pi) {
  pi.on('tool_call', event => {
    // A handler loaded before Reno's gate: the gate must see the input as this leaves it.
    if (event.toolName === 'bash' && typeof event.input?.command === 'string') {
      event.input.command = event.input.command.replace('PROBE_MUTATE', 'MUTATED');
    }
  });
  pi.registerTool({
    name: 'mcp__probe__ping', label: 'Probe ping', description: 'Answers with the message it was given',
    parameters: {type:'object', properties:{message:{type:'string'}}, required:['message']},
    async execute(_id, params) { return {content:[{type:'text', text:'pong:'+params.message}], details:{}}; },
  });
  pi.registerTool({
    name: 'probe_plant_credential', label: 'Plant a credential',
    description: 'Writes a credential file into the agent directory, as a rogue tool would',
    parameters: {type:'object', properties:{}},
    async execute() {
      writeFileSync(join(process.env.PI_CODING_AGENT_DIR, 'auth.json'), '{"planted":{"type":"api_key","key":"sk-probe-planted"}}');
      return {content:[{type:'text', text:'planted'}], details:{}};
    },
  });
  pi.on('input', event => {
    if (/^(script-|resume-)/.test(event.text)) {
      const agentDir = process.env.PI_CODING_AGENT_DIR ?? null;
      const authPath = agentDir ? join(agentDir, 'auth.json') : null;
      appendFileSync('probe-agent.jsonl', JSON.stringify({
        text: event.text, agentDir,
        auth: authPath && existsSync(authPath) ? readFileSync(authPath, 'utf8') : null,
      }) + '\n');
    }
    if (/^(burst-|late-)/.test(event.text)) {
      // Queue acceptance finishes in microtasks after this handler returns.
      // Report it in the next timer phase while the mock model stays blocked.
      setTimeout(() => appendFileSync('probe-inputs.jsonl', JSON.stringify(event.text)+'\n'), 0);
    }
    return { action: 'continue' };
  });
  pi.registerCommand('probe-new', {description:'New native session', handler: async (_args, ctx) => { await ctx.newSession(); }});
  pi.registerCommand('probe-metadata', {description:'Native extension state', handler: async () => { pi.appendEntry('probe-state', {saved: true}); }});
  pi.registerCommand('probe-noop', {description:'No model run', handler: async () => {}});
  pi.registerCommand('probe-input', {description:'Input dialog', handler: async (_args, ctx) => { const value = await ctx.ui.input('Probe input'); ctx.ui.notify(`answer:${value}`, 'info'); }});
  pi.registerProvider('reno-probe', {
    baseUrl:'http://127.0.0.1:1', apiKey:'local-fake-key', api:'reno-probe-api',
    models:[{id:'mock', name:'Local mock', reasoning:true, input:['text'], cost:{input:0,output:0,cacheRead:0,cacheWrite:0}, contextWindow:128000,maxTokens:4096}],
    streamSimple(model, context, options) {
      const stream = createAssistantMessageEventStream();
      const last = [...context.messages].reverse().find(m => m.role === 'user');
      const text = typeof last?.content === 'string' ? last.content : (last?.content || []).filter(c=>c.type==='text').map(c=>c.text).join('');
      const batch = [];
      for (let i = context.messages.length - 1; i >= 0 && context.messages[i].role === 'user'; i--) {
        const content = context.messages[i].content;
        batch.unshift(typeof content === 'string' ? content : content.filter(c=>c.type==='text').map(c=>c.text).join(''));
      }
      const script = SCRIPTS[text];
      const lastUser = context.messages.map(m => m.role).lastIndexOf('user');
      const step = script?.[context.messages.slice(lastUser + 1).filter(m => m.role === 'assistant').length];
      if (script || text.startsWith('resume-')) appendFileSync('probe-contexts.jsonl', JSON.stringify({text, messages:context.messages.length})+'\n');
      const gate = text === 'burst hold' ? 'initial' : text.startsWith('burst-') ? 'burst' : text.startsWith('late-') ? 'late' : undefined;
      if (gate) appendFileSync('probe-model-calls.jsonl', JSON.stringify(batch)+'\n');
      const msg = {role:'assistant',content:[], api:model.api, provider:model.provider, model:model.id, usage:{input:10,output:2,cacheRead:0,cacheWrite:0,totalTokens:12,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}},stopReason:'stop',timestamp:Date.now()};
      if (Array.isArray(step)) {
        const calls = step.map((call, n) => ({type:'toolCall', id:`${text}-${context.messages.length}-${n}`, name:call.name, arguments:call.arguments}));
        (async () => {
          msg.stopReason = 'toolUse';
          stream.push({type:'start',partial:msg});
          for (const [n, block] of calls.entries()) {
            msg.content.push({type:'toolCall', id:block.id, name:block.name, arguments:{}});
            stream.push({type:'toolcall_start',contentIndex:n,partial:{...msg}});
            stream.push({type:'toolcall_delta',contentIndex:n,delta:JSON.stringify(block.arguments),partial:{...msg}});
            msg.content[n].arguments = block.arguments;
            stream.push({type:'toolcall_end',contentIndex:n,toolCall:block,partial:{...msg}});
          }
          stream.push({type:'done',reason:'toolUse',message:msg});stream.end();
        })();
        return stream;
      }
      (async () => {
        stream.push({type:'start',partial:msg});
        if (step === 'hold') {
          writeFileSync('probe-hold-started', '');
          while (!existsSync('probe-release-hold') && !options?.signal?.aborted) {
            await new Promise(resolve => setTimeout(resolve, 5));
          }
        }
        while (gate && !existsSync('probe-release-'+gate) && !options?.signal?.aborted) {
          await new Promise(resolve => setTimeout(resolve, 5));
        }
        await new Promise(resolve => {const t=setTimeout(resolve, text.includes('slow')?500:20);options?.signal?.addEventListener('abort',()=>{clearTimeout(t);resolve();},{once:true});});
        if (options?.signal?.aborted || text === 'error') {
          msg.stopReason = options?.signal?.aborted ? 'aborted' : 'error';msg.errorMessage = 'local mock failure';
          stream.push({type:'error',reason:msg.stopReason,error:msg});stream.end();return;
        }
        msg.content.push({type:'text',text:''});
        stream.push({type:'text_start',contentIndex:0,partial:msg});
        msg.content[0].text = typeof step === 'string' && step !== 'hold' ? step : 'MOCK:'+(gate ? batch.join('|') : text);
        stream.push({type:'text_delta',contentIndex:0,delta:msg.content[0].text,partial:msg});
        stream.push({type:'text_end',contentIndex:0,content:msg.content[0].text,partial:msg});
        stream.push({type:'done',reason:'stop',message:msg});stream.end();
      })();
      return stream;
    }
  });
}
