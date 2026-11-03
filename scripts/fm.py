import sys,os,re
for root in ("reviews","evidence"):
    for d,_,fs in os.walk(root):
        for f in fs:
            if not f.endswith(".md"): continue
            p=os.path.join(d,f); s=open(p).read()
            if s.startswith("---"): continue
            t=f[:-3].replace("-"," ")
            open(p,"w").write(f"---\ntitle: {t}\ntype: reference\nstatus: draft\n---\n\n"+s)
            print("fm",p)
