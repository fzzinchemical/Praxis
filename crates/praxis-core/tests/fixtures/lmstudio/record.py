import json,time,random,sys,os,urllib.request,urllib.error
B="http://127.0.0.1:1234"; OUT=sys.argv[1]; CTX=8192
op=urllib.request.build_opener(urllib.request.ProxyHandler({}))
MODELS=["qwen/qwen2.5-coder-7b-instruct","google/gemma-4-12b-qat"]
TOOLS=[{"type":"function","function":{"name":"Read","description":"Read a file","parameters":{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}}},
{"type":"function","function":{"name":"Write","description":"Write a file","parameters":{"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}},"required":["file_path","content"]}}}]
results=[]
def rec(**k): results.append(k); print(json.dumps(k)[:400],flush=True); json.dump(results,open(f"{OUT}/results.json","w"),indent=1)
def api(m,p,b=None,t=600):
    r=urllib.request.Request(B+p,method=m,data=json.dumps(b).encode() if b is not None else None,headers={"Content-Type":"application/json"})
    try:
        with op.open(r,timeout=t) as f: return f.status,json.loads(f.read())
    except urllib.error.HTTPError as e: return e.code,e.read().decode(errors="replace")
def chat(model,msgs,save=None,**kw):
    body={"model":model,"messages":msgs,"stream":True,"stream_options":{"include_usage":True},"temperature":0,"seed":42,**kw}
    t0=time.time(); ttft=None; raw=b""; status=None; usage=None
    r=urllib.request.Request(B+"/v1/chat/completions",data=json.dumps(body).encode(),headers={"Content-Type":"application/json"})
    try:
        with op.open(r,timeout=900) as f:
            status=f.status
            for line in f:
                raw+=line
                if ttft is None and line.startswith(b"data:") and b'"delta"' in line and b"[DONE]" not in line: ttft=time.time()-t0
                if line.startswith(b"data: {") and b'"usage"' in line:
                    try:
                        u=json.loads(line[6:]).get("usage")
                        if u: usage=u
                    except ValueError: pass
    except urllib.error.HTTPError as e: status=e.code; raw=e.read()
    except Exception as e: status="EXC:"+repr(e)
    if save:
        d=f"{OUT}/fixtures/{model.split('/')[-1].split(':')[0]}"; os.makedirs(d,exist_ok=True); open(f"{d}/{save}.sse","wb").write(raw)
        # long synthetic prompts are recorded by size only; responses stay byte-exact
        b2=dict(body); b2["messages"]=[{**m,"content":m["content"] if len(m["content"])<2000 else f"<{len(m['content'])} chars synthetic>"} for m in msgs]
        json.dump(b2,open(f"{d}/{save}.request.json","w"),indent=1)
    return dict(status=status,ttft=ttft,total=time.time()-t0,usage=usage,bytes=len(raw)),raw
def filler(chars,seed):
    r=random.Random(seed); w="fn let struct parse token buffer index config value return match error stream agent tool model file path".split(); o=[]; n=0
    while n<chars:
        l=f"{r.choice(w)}_{r.randint(0,9999)} {r.choice(w)} {r.choice(w)}_{r.randint(0,99)}\n"; o.append(l); n+=len(l)
    return "".join(o)[:chars]
print(api("GET","/api/v1/models")[0])
for M in MODELS:
    s,l=api("POST","/api/v1/models/load",{"model":M,"context_length":CTX,"echo_load_config":True}); rec(case="load",model=M,status=s,resp=l)
    d=f"{OUT}/fixtures/{M.split('/')[-1]}"; os.makedirs(d,exist_ok=True); json.dump(l,open(f"{d}/load.response.json","w"),indent=1)
    if s!=200: continue
    iid=l.get("instance_id",M)
    st,ml=api("GET","/api/v1/models"); json.dump([m for m in ml["models"] if m["key"]==M],open(f"{d}/models.response.json","w"),indent=1)
    # tools
    for name,p in [("tool_simple","Read the file src/main.rs using the tool."),("tool_parallel","Read both src/a.rs and src/b.rs using the tool."),("tool_multiline",'Use the Write tool to create hello.py containing a function that prints "hi" on two separate lines with quotes.'),("no_tool_needed","What is 2+2? Answer in one word.")]:
        for i in range(2):
            r,raw=chat(M,[{"role":"user","content":p}],save=f"{name}_{i}",tools=TOOLS,max_tokens=1024)
            rec(case=name,run=i,model=M,**r,has_native=b'"tool_calls"' in raw,has_reasoning=b'reasoning' in raw,has_think=b'think' in raw,has_toolcall_tag=b'tool_call>' in raw)
    # calibrate chars per token
    r,_=chat(M,[{"role":"user","content":filler(8000,1)+"\nSay ok."}],save="calibrate",max_tokens=8)
    if not r["usage"]: rec(case="calibrate_fail",model=M,**r); cpt=3.0
    else: cpt=8000/r["usage"]["prompt_tokens"]
    rec(case="calibrate",model=M,cpt=cpt,**r)
    # thresholds
    for ratio in [0.5,0.9,0.99,1.01,1.1,1.5,3.0]:
        n=int(ratio*CTX*cpt); r,raw=chat(M,[{"role":"user","content":filler(n,2)+"\nSay ok."}],save=f"oversize_{ratio}",max_tokens=16)
        rec(case="oversize",model=M,ratio=ratio,est_tokens=int(n/cpt),err=raw[:300].decode(errors="replace") if r["usage"] is None else None,**r)
    # prefix reuse: cold, extended, extended, then changed-start control; 3 rounds
    pc=int(5000*cpt)
    for rnd in range(3):
        base=filler(pc,100+rnd)
        seq=[("cold",base+"\nQ1 say ok."),("shared_prefix",base+"\nQ2 say yes."),("shared_prefix2",base+"\nQ3 say no."),("changed_start","X"+base[1:]+"\nQ4 say ok.")]
        for lab,c in seq:
            r,raw=chat(M,[{"role":"user","content":c}],save=f"prefix_{lab}_{rnd}",max_tokens=8)
            rec(case="prefix",round=rnd,label=lab,model=M,**r)
    # tools change at start of prompt
    base=filler(pc,200)
    for lab,t in [("tools_a",TOOLS),("tools_a_again",TOOLS),("tools_changed",TOOLS[:1])]:
        r,raw=chat(M,[{"role":"user","content":base+"\nsay ok"}],tools=t,max_tokens=8); rec(case="tools_prefix",label=lab,model=M,**r)
    # constrained
    schema={"type":"json_schema","json_schema":{"name":"read","strict":True,"schema":TOOLS[0]["function"]["parameters"]}}
    r,raw=chat(M,[{"role":"user","content":"Give arguments to read src/main.rs."}],save="response_format",response_format=schema,max_tokens=256); rec(case="response_format",model=M,**r)
    r,raw=chat(M,[{"role":"user","content":"Read src/main.rs."}],save="response_format_with_tools",response_format=schema,tools=TOOLS,max_tokens=256); rec(case="response_format_tools",model=M,**r,has_native=b'"tool_calls"' in raw)
    # instance selection
    s2,l2=api("POST","/api/v1/models/load",{"model":M,"context_length":4096,"echo_load_config":True}); rec(case="load2",model=M,status=s2,resp=l2)
    iid2=l2.get("instance_id") if s2==200 else None
    st,ml=api("GET","/api/v1/models"); rec(case="instances",model=M,inst=[m["loaded_instances"] for m in ml["models"] if m["key"]==M])
    big=filler(int(6000*cpt),9)+"\nsay ok"
    for lab,mid in [("first",iid),("second_4k",iid2),("key",M),("bogus","nope/none")]:
        if mid is None: continue
        r,raw=chat(mid,[{"role":"user","content":big}],save=None,max_tokens=4)
        ml2=None
        for ln in raw.splitlines():
            if ln.startswith(b"data: {"):
                try: ml2=json.loads(ln[6:]).get("model"); break
                except ValueError: pass
        rec(case="instance_select",label=lab,sent=mid,echoed=ml2,model=M,err=raw[:300].decode(errors="replace") if r["usage"] is None else None,**r)
    for i in [iid,iid2]:
        if i: rec(case="unload",model=M,instance=i,r=api("POST","/api/v1/models/unload",{"instance_id":i}))
s,r=api("POST","/v1/chat/completions",{"model":"nope","messages":[{"role":"user","content":"x"}]}); rec(case="no_model_loaded",status=s,resp=r)
print("DONE")
