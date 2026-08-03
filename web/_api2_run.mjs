import * as ed from "@noble/ed25519";
const API="http://localhost:8930";
const DEMO="68756c6c2d64656d6f2d6f776e65722d6b65792d64656d6f2d6f6e6c79212121";
const hx=h=>Uint8Array.from((h.match(/../g)??[]).map(x=>parseInt(x,16)));
const bh=b=>[...b].map(x=>x.toString(16).padStart(2,"0")).join("");
const sk=hx(DEMO), actor=bh(await ed.getPublicKeyAsync(sk));
const {nonce}=await fetch(`${API}/api/auth/challenge`).then(r=>r.json());
const sig=bh(await ed.signAsync(new TextEncoder().encode(`hull-login:${nonce}`),sk));
const {token}=await fetch(`${API}/api/auth/login`,{method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify({actor,nonce,signature:sig})}).then(r=>r.json());
const H={authorization:`Bearer ${token}`,"content-type":"application/json"};
// admin can view settings
const st=await fetch(`${API}/api/repos/tankrap/hull/settings`,{headers:H});
console.log("admin settings GET:", st.status);
// create an empty repo
const cr=await fetch(`${API}/api/repos`,{method:"POST",headers:H,body:JSON.stringify({account:"tankrap",name:"sandbox-demo"})});
console.log("create repo:", cr.status, (await cr.text()).slice(0,120));
// import a github repo (small/temp)
const im=await fetch(`${API}/api/repos/import`,{method:"POST",headers:H,body:JSON.stringify({account:"tankrap",source:"tankrap/goit-temp-repo",name:"goit-temp"})});
console.log("import:", im.status, (await im.text()).slice(0,160));
