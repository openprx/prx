var Ls=Object.defineProperty;var no=e=>{throw TypeError(e)};var Fs=(e,t,r)=>t in e?Ls(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var er=(e,t,r)=>Fs(e,typeof t!="symbol"?t+"":t,r),gn=(e,t,r)=>t.has(e)||no("Cannot "+r);var E=(e,t,r)=>(gn(e,t,"read from private field"),r?r.call(e):t.get(e)),We=(e,t,r)=>t.has(e)?no("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Le=(e,t,r,n)=>(gn(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),Et=(e,t,r)=>(gn(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const o of document.querySelectorAll('link[rel="modulepreload"]'))n(o);new MutationObserver(o=>{for(const s of o)if(s.type==="childList")for(const l of s.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(o){const s={};return o.integrity&&(s.integrity=o.integrity),o.referrerPolicy&&(s.referrerPolicy=o.referrerPolicy),o.crossOrigin==="use-credentials"?s.credentials="include":o.crossOrigin==="anonymous"?s.credentials="omit":s.credentials="same-origin",s}function n(o){if(o.ep)return;o.ep=!0;const s=r(o);fetch(o.href,s)}})();const wn=!1;var Bn=Array.isArray,Rs=Array.prototype.indexOf,va=Array.prototype.includes,rn=Array.from,js=Object.defineProperty,Pr=Object.getOwnPropertyDescriptor,Hs=Object.getOwnPropertyDescriptors,Ds=Object.prototype,zs=Array.prototype,Co=Object.getPrototypeOf,oo=Object.isExtensible;function Ea(e){return typeof e=="function"}const Ce=()=>{};function Us(e){for(var t=0;t<e.length;t++)e[t]()}function Mo(){var e,t,r=new Promise((n,o)=>{e=n,t=o});return{promise:r,resolve:e,reject:t}}function Na(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const It=2,_a=4,ga=8,an=1<<24,jr=16,sr=32,Zr=64,Sn=128,Xt=512,Tt=1024,Pt=2048,or=4096,jt=8192,gr=16384,xa=32768,Sr=65536,so=1<<17,Bs=1<<18,ka=1<<19,Ws=1<<20,fr=1<<25,Yr=65536,An=1<<21,Wn=1<<22,Or=1<<23,Ir=Symbol("$state"),No=Symbol("legacy props"),Vs=Symbol(""),Hr=new class extends Error{constructor(){super(...arguments);er(this,"name","StaleReactionError");er(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var Ao;const Vn=!!((Ao=globalThis.document)!=null&&Ao.contentType)&&globalThis.document.contentType.includes("xml");function To(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function qs(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function Gs(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function Ks(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function Js(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Ys(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Xs(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Qs(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function Zs(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function ei(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function ti(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function ri(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const ai=1,ni=2,Po=4,oi=8,si=16,ii=1,li=4,di=8,ci=16,ui=1,fi=2,Ct=Symbol(),Oo="http://www.w3.org/1999/xhtml",Io="http://www.w3.org/2000/svg",vi="http://www.w3.org/1998/Math/MathML",gi="@attach";function pi(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function bi(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Lo(e){return e===this.v}function yi(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Fo(e){return!yi(e,this.v)}let hi=!1,Wt=null;function pa(e){Wt=e}function Pe(e,t=!1,r){Wt={p:Wt,i:!1,c:null,e:null,s:e,x:null,l:null}}function Oe(e){var t=Wt,r=t.e;if(r!==null){t.e=null;for(var n of r)as(n)}return t.i=!0,Wt=t.p,{}}function Ro(){return!0}let Dr=[];function jo(){var e=Dr;Dr=[],Us(e)}function pr(e){if(Dr.length===0&&!Pa){var t=Dr;queueMicrotask(()=>{t===Dr&&jo()})}Dr.push(e)}function mi(){for(;Dr.length>0;)jo()}function Ho(e){var t=Xe;if(t===null)return De.f|=Or,e;if(!(t.f&xa)&&!(t.f&_a))throw e;Tr(e,t)}function Tr(e,t){for(;t!==null;){if(t.f&Sn){if(!(t.f&xa))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const _i=-7169;function wt(e,t){e.f=e.f&_i|t}function qn(e){e.f&Xt||e.deps===null?wt(e,Tt):wt(e,or)}function Do(e){if(e!==null)for(const t of e)!(t.f&It)||!(t.f&Yr)||(t.f^=Yr,Do(t.deps))}function zo(e,t,r){e.f&Pt?t.add(e):e.f&or&&r.add(e),Do(e.deps),wt(e,Tt)}const Wa=new Set;let Me=null,Ya=null,Nt=null,zt=[],nn=null,Pa=!1,ba=null,xi=1;var Cr,oa,Wr,sa,ia,la,Mr,lr,da,Vt,En,$n,Cn,Mn;const ao=class ao{constructor(){We(this,Vt);er(this,"id",xi++);er(this,"current",new Map);er(this,"previous",new Map);We(this,Cr,new Set);We(this,oa,new Set);We(this,Wr,0);We(this,sa,0);We(this,ia,null);We(this,la,new Set);We(this,Mr,new Set);We(this,lr,new Map);er(this,"is_fork",!1);We(this,da,!1)}skip_effect(t){E(this,lr).has(t)||E(this,lr).set(t,{d:[],m:[]})}unskip_effect(t){var r=E(this,lr).get(t);if(r){E(this,lr).delete(t);for(var n of r.d)wt(n,Pt),vr(n);for(n of r.m)wt(n,or),vr(n)}}process(t){var o;zt=[],this.apply();var r=ba=[],n=[];for(const s of t)Et(this,Vt,$n).call(this,s,r,n);if(ba=null,Et(this,Vt,En).call(this)){Et(this,Vt,Cn).call(this,n),Et(this,Vt,Cn).call(this,r);for(const[s,l]of E(this,lr))Vo(s,l)}else{Ya=this,Me=null;for(const s of E(this,Cr))s(this);E(this,Cr).clear(),E(this,Wr)===0&&Et(this,Vt,Mn).call(this),io(n),io(r),E(this,la).clear(),E(this,Mr).clear(),Ya=null,(o=E(this,ia))==null||o.resolve()}Nt=null}capture(t,r){r!==Ct&&!this.previous.has(t)&&this.previous.set(t,r),t.f&Or||(this.current.set(t,t.v),Nt==null||Nt.set(t,t.v))}activate(){Me=this,this.apply()}deactivate(){Me===this&&(Me=null,Nt=null)}flush(){var t;if(zt.length>0)Me=this,Uo();else if(E(this,Wr)===0&&!this.is_fork){for(const r of E(this,Cr))r(this);E(this,Cr).clear(),Et(this,Vt,Mn).call(this),(t=E(this,ia))==null||t.resolve()}this.deactivate()}discard(){for(const t of E(this,oa))t(this);E(this,oa).clear()}increment(t){Le(this,Wr,E(this,Wr)+1),t&&Le(this,sa,E(this,sa)+1)}decrement(t){Le(this,Wr,E(this,Wr)-1),t&&Le(this,sa,E(this,sa)-1),!E(this,da)&&(Le(this,da,!0),pr(()=>{Le(this,da,!1),Et(this,Vt,En).call(this)?zt.length>0&&this.flush():this.revive()}))}revive(){for(const t of E(this,la))E(this,Mr).delete(t),wt(t,Pt),vr(t);for(const t of E(this,Mr))wt(t,or),vr(t);this.flush()}oncommit(t){E(this,Cr).add(t)}ondiscard(t){E(this,oa).add(t)}settled(){return(E(this,ia)??Le(this,ia,Mo())).promise}static ensure(){if(Me===null){const t=Me=new ao;Wa.add(Me),Pa||pr(()=>{Me===t&&t.flush()})}return Me}apply(){}};Cr=new WeakMap,oa=new WeakMap,Wr=new WeakMap,sa=new WeakMap,ia=new WeakMap,la=new WeakMap,Mr=new WeakMap,lr=new WeakMap,da=new WeakMap,Vt=new WeakSet,En=function(){return this.is_fork||E(this,sa)>0},$n=function(t,r,n){t.f^=Tt;for(var o=t.first;o!==null;){var s=o.f,l=(s&(sr|Zr))!==0,d=l&&(s&Tt)!==0,c=(s&jt)!==0,f=d||E(this,lr).has(o);if(!f&&o.fn!==null){l?c||(o.f^=Tt):s&_a?r.push(o):s&(ga|an)&&c?n.push(o):Ua(o)&&(ha(o),s&jr&&(E(this,Mr).add(o),c&&wt(o,Pt)));var h=o.first;if(h!==null){o=h;continue}}for(;o!==null;){var k=o.next;if(k!==null){o=k;break}o=o.parent}}},Cn=function(t){for(var r=0;r<t.length;r+=1)zo(t[r],E(this,la),E(this,Mr))},Mn=function(){var s;if(Wa.size>1){this.previous.clear();var t=Me,r=Nt,n=!0;for(const l of Wa){if(l===this){n=!1;continue}const d=[];for(const[f,h]of this.current){if(l.current.has(f))if(n&&h!==l.current.get(f))l.current.set(f,h);else continue;d.push(f)}if(d.length===0)continue;const c=[...l.current.keys()].filter(f=>!this.current.has(f));if(c.length>0){var o=zt;zt=[];const f=new Set,h=new Map;for(const k of d)Bo(k,c,f,h);if(zt.length>0){Me=l,l.apply();for(const k of zt)Et(s=l,Vt,$n).call(s,k,[],[]);l.deactivate()}zt=o}}Me=t,Nt=r}E(this,lr).clear(),Wa.delete(this)};let Lr=ao;function ki(e){var t=Pa;Pa=!0;try{for(var r;;){if(mi(),zt.length===0&&(Me==null||Me.flush(),zt.length===0))return nn=null,r;Uo()}}finally{Pa=t}}function Uo(){var e=null;try{for(var t=0;zt.length>0;){var r=Lr.ensure();if(t++>1e3){var n,o;wi()}r.process(zt),Fr.clear()}}finally{zt=[],nn=null,ba=null}}function wi(){try{Xs()}catch(e){Tr(e,nn)}}let tr=null;function io(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(gr|jt))&&Ua(n)&&(tr=new Set,ha(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&is(n),(tr==null?void 0:tr.size)>0)){Fr.clear();for(const o of tr){if(o.f&(gr|jt))continue;const s=[o];let l=o.parent;for(;l!==null;)tr.has(l)&&(tr.delete(l),s.push(l)),l=l.parent;for(let d=s.length-1;d>=0;d--){const c=s[d];c.f&(gr|jt)||ha(c)}}tr.clear()}}tr=null}}function Bo(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const o of e.reactions){const s=o.f;s&It?Bo(o,t,r,n):s&(Wn|jr)&&!(s&Pt)&&Wo(o,t,n)&&(wt(o,Pt),vr(o))}}function Wo(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const o of e.deps){if(va.call(t,o))return!0;if(o.f&It&&Wo(o,t,r))return r.set(o,!0),!0}return r.set(e,!1),!1}function vr(e){var t=nn=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(_a|ga|an)&&!(e.f&xa)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(ba!==null&&t===Xe&&!(e.f&ga))return;if(n&(Zr|sr)){if(!(n&Tt))return;t.f^=Tt}}zt.push(t)}function Vo(e,t){if(!(e.f&sr&&e.f&Tt)){e.f&Pt?t.d.push(e):e.f&or&&t.m.push(e),wt(e,Tt);for(var r=e.first;r!==null;)Vo(r,t),r=r.next}}function Si(e){let t=0,r=Xr(0),n;return()=>{Jn()&&(a(r),Yn(()=>(t===0&&(n=Sa(()=>e(()=>Oa(r)))),t+=1,()=>{pr(()=>{t-=1,t===0&&(n==null||n(),n=void 0,Oa(r))})})))}}var Ai=Sr|ka;function Ei(e,t,r,n){new $i(e,t,r,n)}var Yt,Un,dr,Vr,Dt,cr,Gt,rr,mr,qr,Nr,ca,ua,fa,_r,en,$t,Ci,Mi,Ni,Nn,Ga,Ka,Tn;class $i{constructor(t,r,n,o){We(this,$t);er(this,"parent");er(this,"is_pending",!1);er(this,"transform_error");We(this,Yt);We(this,Un,null);We(this,dr);We(this,Vr);We(this,Dt);We(this,cr,null);We(this,Gt,null);We(this,rr,null);We(this,mr,null);We(this,qr,0);We(this,Nr,0);We(this,ca,!1);We(this,ua,new Set);We(this,fa,new Set);We(this,_r,null);We(this,en,Si(()=>(Le(this,_r,Xr(E(this,qr))),()=>{Le(this,_r,null)})));var s;Le(this,Yt,t),Le(this,dr,r),Le(this,Vr,l=>{var d=Xe;d.b=this,d.f|=Sn,n(l)}),this.parent=Xe.b,this.transform_error=o??((s=this.parent)==null?void 0:s.transform_error)??(l=>l),Le(this,Dt,wa(()=>{Et(this,$t,Nn).call(this)},Ai))}defer_effect(t){zo(t,E(this,ua),E(this,fa))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!E(this,dr).pending}update_pending_count(t){Et(this,$t,Tn).call(this,t),Le(this,qr,E(this,qr)+t),!(!E(this,_r)||E(this,ca))&&(Le(this,ca,!0),pr(()=>{Le(this,ca,!1),E(this,_r)&&ya(E(this,_r),E(this,qr))}))}get_effect_pending(){return E(this,en).call(this),a(E(this,_r))}error(t){var r=E(this,dr).onerror;let n=E(this,dr).failed;if(!r&&!n)throw t;E(this,cr)&&(Ot(E(this,cr)),Le(this,cr,null)),E(this,Gt)&&(Ot(E(this,Gt)),Le(this,Gt,null)),E(this,rr)&&(Ot(E(this,rr)),Le(this,rr,null));var o=!1,s=!1;const l=()=>{if(o){bi();return}o=!0,s&&ri(),E(this,rr)!==null&&Kr(E(this,rr),()=>{Le(this,rr,null)}),Et(this,$t,Ka).call(this,()=>{Lr.ensure(),Et(this,$t,Nn).call(this)})},d=c=>{try{s=!0,r==null||r(c,l),s=!1}catch(f){Tr(f,E(this,Dt)&&E(this,Dt).parent)}n&&Le(this,rr,Et(this,$t,Ka).call(this,()=>{Lr.ensure();try{return Bt(()=>{var f=Xe;f.b=this,f.f|=Sn,n(E(this,Yt),()=>c,()=>l)})}catch(f){return Tr(f,E(this,Dt).parent),null}}))};pr(()=>{var c;try{c=this.transform_error(t)}catch(f){Tr(f,E(this,Dt)&&E(this,Dt).parent);return}c!==null&&typeof c=="object"&&typeof c.then=="function"?c.then(d,f=>Tr(f,E(this,Dt)&&E(this,Dt).parent)):d(c)})}}Yt=new WeakMap,Un=new WeakMap,dr=new WeakMap,Vr=new WeakMap,Dt=new WeakMap,cr=new WeakMap,Gt=new WeakMap,rr=new WeakMap,mr=new WeakMap,qr=new WeakMap,Nr=new WeakMap,ca=new WeakMap,ua=new WeakMap,fa=new WeakMap,_r=new WeakMap,en=new WeakMap,$t=new WeakSet,Ci=function(){try{Le(this,cr,Bt(()=>E(this,Vr).call(this,E(this,Yt))))}catch(t){this.error(t)}},Mi=function(t){const r=E(this,dr).failed;r&&Le(this,rr,Bt(()=>{r(E(this,Yt),()=>t,()=>()=>{})}))},Ni=function(){const t=E(this,dr).pending;t&&(this.is_pending=!0,Le(this,Gt,Bt(()=>t(E(this,Yt)))),pr(()=>{var r=Le(this,mr,document.createDocumentFragment()),n=kr();r.append(n),Le(this,cr,Et(this,$t,Ka).call(this,()=>(Lr.ensure(),Bt(()=>E(this,Vr).call(this,n))))),E(this,Nr)===0&&(E(this,Yt).before(r),Le(this,mr,null),Kr(E(this,Gt),()=>{Le(this,Gt,null)}),Et(this,$t,Ga).call(this))}))},Nn=function(){try{if(this.is_pending=this.has_pending_snippet(),Le(this,Nr,0),Le(this,qr,0),Le(this,cr,Bt(()=>{E(this,Vr).call(this,E(this,Yt))})),E(this,Nr)>0){var t=Le(this,mr,document.createDocumentFragment());Zn(E(this,cr),t);const r=E(this,dr).pending;Le(this,Gt,Bt(()=>r(E(this,Yt))))}else Et(this,$t,Ga).call(this)}catch(r){this.error(r)}},Ga=function(){this.is_pending=!1;for(const t of E(this,ua))wt(t,Pt),vr(t);for(const t of E(this,fa))wt(t,or),vr(t);E(this,ua).clear(),E(this,fa).clear()},Ka=function(t){var r=Xe,n=De,o=Wt;br(E(this,Dt)),Zt(E(this,Dt)),pa(E(this,Dt).ctx);try{return t()}catch(s){return Ho(s),null}finally{br(r),Zt(n),pa(o)}},Tn=function(t){var r;if(!this.has_pending_snippet()){this.parent&&Et(r=this.parent,$t,Tn).call(r,t);return}Le(this,Nr,E(this,Nr)+t),E(this,Nr)===0&&(Et(this,$t,Ga).call(this),E(this,Gt)&&Kr(E(this,Gt),()=>{Le(this,Gt,null)}),E(this,mr)&&(E(this,Yt).before(E(this,mr)),Le(this,mr,null)))};function qo(e,t,r,n){const o=on;var s=e.filter(k=>!k.settled);if(r.length===0&&s.length===0){n(t.map(o));return}var l=Xe,d=Ti(),c=s.length===1?s[0].promise:s.length>1?Promise.all(s.map(k=>k.promise)):null;function f(k){d();try{n(k)}catch(w){l.f&gr||Tr(w,l)}Pn()}if(r.length===0){c.then(()=>f(t.map(o)));return}function h(){d(),Promise.all(r.map(k=>Oi(k))).then(k=>f([...t.map(o),...k])).catch(k=>Tr(k,l))}c?c.then(h):h()}function Ti(){var e=Xe,t=De,r=Wt,n=Me;return function(s=!0){br(e),Zt(t),pa(r),s&&(n==null||n.activate())}}function Pn(e=!0){br(null),Zt(null),pa(null),e&&(Me==null||Me.deactivate())}function Pi(){var e=Xe.b,t=Me,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function on(e){var t=It|Pt,r=De!==null&&De.f&It?De:null;return Xe!==null&&(Xe.f|=ka),{ctx:Wt,deps:null,effects:null,equals:Lo,f:t,fn:e,reactions:null,rv:0,v:Ct,wv:0,parent:r??Xe,ac:null}}function Oi(e,t,r){Xe===null&&qs();var o=void 0,s=Xr(Ct),l=!De,d=new Map;return Gi(()=>{var w;var c=Mo();o=c.promise;try{Promise.resolve(e()).then(c.resolve,c.reject).finally(Pn)}catch(L){c.reject(L),Pn()}var f=Me;if(l){var h=Pi();(w=d.get(f))==null||w.reject(Hr),d.delete(f),d.set(f,c)}const k=(L,T=void 0)=>{if(f.activate(),T)T!==Hr&&(s.f|=Or,ya(s,T));else{s.f&Or&&(s.f^=Or),ya(s,L);for(const[j,N]of d){if(d.delete(j),j===f)break;N.reject(Hr)}}h&&h()};c.promise.then(k,L=>k(null,L||"unknown"))}),ln(()=>{for(const c of d.values())c.reject(Hr)}),new Promise(c=>{function f(h){function k(){h===o?c(s):f(o)}h.then(k,k)}f(o)})}function te(e){const t=on(e);return cs(t),t}function Go(e){const t=on(e);return t.equals=Fo,t}function Ii(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)Ot(t[r])}}function Li(e){for(var t=e.parent;t!==null;){if(!(t.f&It))return t.f&gr?null:t;t=t.parent}return null}function Gn(e){var t,r=Xe;br(Li(e));try{e.f&=~Yr,Ii(e),t=gs(e)}finally{br(r)}return t}function Ko(e){var t=Gn(e);if(!e.equals(t)&&(e.wv=fs(),(!(Me!=null&&Me.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){wt(e,Tt);return}Rr||(Nt!==null?(Jn()||Me!=null&&Me.is_fork)&&Nt.set(e,t):qn(e))}function Fi(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(Hr),n.teardown=Ce,n.ac=null,La(n,0),Xn(n))}function Jo(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&ha(t)}let On=new Set;const Fr=new Map;let Yo=!1;function Xr(e,t){var r={f:0,v:e,reactions:null,equals:Lo,rv:0,wv:0};return r}function R(e,t){const r=Xr(e);return cs(r),r}function Ri(e,t=!1,r=!0){const n=Xr(e);return t||(n.equals=Fo),n}function p(e,t,r=!1){De!==null&&(!nr||De.f&so)&&Ro()&&De.f&(It|jr|Wn|so)&&(Qt===null||!va.call(Qt,e))&&ti();let n=r?mt(t):t;return ya(e,n)}function ya(e,t){if(!e.equals(t)){var r=e.v;Rr?Fr.set(e,t):Fr.set(e,r),e.v=t;var n=Lr.ensure();if(n.capture(e,r),e.f&It){const o=e;e.f&Pt&&Gn(o),qn(o)}e.wv=fs(),Xo(e,Pt),Xe!==null&&Xe.f&Tt&&!(Xe.f&(sr|Zr))&&(Jt===null?Ji([e]):Jt.push(e)),!n.is_fork&&On.size>0&&!Yo&&ji()}return t}function ji(){Yo=!1;for(const e of On)e.f&Tt&&wt(e,or),Ua(e)&&ha(e);On.clear()}function Oa(e){p(e,e.v+1)}function Xo(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,o=0;o<n;o++){var s=r[o],l=s.f,d=(l&Pt)===0;if(d&&wt(s,t),l&It){var c=s;Nt==null||Nt.delete(c),l&Yr||(l&Xt&&(s.f|=Yr),Xo(c,or))}else d&&(l&jr&&tr!==null&&tr.add(s),vr(s))}}function mt(e){if(typeof e!="object"||e===null||Ir in e)return e;const t=Co(e);if(t!==Ds&&t!==zs)return e;var r=new Map,n=Bn(e),o=R(0),s=Jr,l=d=>{if(Jr===s)return d();var c=De,f=Jr;Zt(null),vo(s);var h=d();return Zt(c),vo(f),h};return n&&r.set("length",R(e.length)),new Proxy(e,{defineProperty(d,c,f){(!("value"in f)||f.configurable===!1||f.enumerable===!1||f.writable===!1)&&Zs();var h=r.get(c);return h===void 0?l(()=>{var k=R(f.value);return r.set(c,k),k}):p(h,f.value,!0),!0},deleteProperty(d,c){var f=r.get(c);if(f===void 0){if(c in d){const h=l(()=>R(Ct));r.set(c,h),Oa(o)}}else p(f,Ct),Oa(o);return!0},get(d,c,f){var L;if(c===Ir)return e;var h=r.get(c),k=c in d;if(h===void 0&&(!k||(L=Pr(d,c))!=null&&L.writable)&&(h=l(()=>{var T=mt(k?d[c]:Ct),j=R(T);return j}),r.set(c,h)),h!==void 0){var w=a(h);return w===Ct?void 0:w}return Reflect.get(d,c,f)},getOwnPropertyDescriptor(d,c){var f=Reflect.getOwnPropertyDescriptor(d,c);if(f&&"value"in f){var h=r.get(c);h&&(f.value=a(h))}else if(f===void 0){var k=r.get(c),w=k==null?void 0:k.v;if(k!==void 0&&w!==Ct)return{enumerable:!0,configurable:!0,value:w,writable:!0}}return f},has(d,c){var w;if(c===Ir)return!0;var f=r.get(c),h=f!==void 0&&f.v!==Ct||Reflect.has(d,c);if(f!==void 0||Xe!==null&&(!h||(w=Pr(d,c))!=null&&w.writable)){f===void 0&&(f=l(()=>{var L=h?mt(d[c]):Ct,T=R(L);return T}),r.set(c,f));var k=a(f);if(k===Ct)return!1}return h},set(d,c,f,h){var q;var k=r.get(c),w=c in d;if(n&&c==="length")for(var L=f;L<k.v;L+=1){var T=r.get(L+"");T!==void 0?p(T,Ct):L in d&&(T=l(()=>R(Ct)),r.set(L+"",T))}if(k===void 0)(!w||(q=Pr(d,c))!=null&&q.writable)&&(k=l(()=>R(void 0)),p(k,mt(f)),r.set(c,k));else{w=k.v!==Ct;var j=l(()=>mt(f));p(k,j)}var N=Reflect.getOwnPropertyDescriptor(d,c);if(N!=null&&N.set&&N.set.call(h,f),!w){if(n&&typeof c=="string"){var I=r.get("length"),X=Number(c);Number.isInteger(X)&&X>=I.v&&p(I,X+1)}Oa(o)}return!0},ownKeys(d){a(o);var c=Reflect.ownKeys(d).filter(k=>{var w=r.get(k);return w===void 0||w.v!==Ct});for(var[f,h]of r)h.v!==Ct&&!(f in d)&&c.push(f);return c},setPrototypeOf(){ei()}})}function lo(e){try{if(e!==null&&typeof e=="object"&&Ir in e)return e[Ir]}catch{}return e}function Hi(e,t){return Object.is(lo(e),lo(t))}var co,Qo,Zo,es;function Di(){if(co===void 0){co=window,Qo=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;Zo=Pr(t,"firstChild").get,es=Pr(t,"nextSibling").get,oo(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),oo(r)&&(r.__t=void 0)}}function kr(e=""){return document.createTextNode(e)}function wr(e){return Zo.call(e)}function za(e){return es.call(e)}function i(e,t){return wr(e)}function Ee(e,t=!1){{var r=wr(e);return r instanceof Comment&&r.data===""?za(r):r}}function g(e,t=1,r=!1){let n=e;for(;t--;)n=za(n);return n}function zi(e){e.textContent=""}function ts(){return!1}function Kn(e,t,r){return document.createElementNS(t??Oo,e,void 0)}function Ui(e,t){if(t){const r=document.body;e.autofocus=!0,pr(()=>{document.activeElement===r&&e.focus()})}}let uo=!1;function Bi(){uo||(uo=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function sn(e){var t=De,r=Xe;Zt(null),br(null);try{return e()}finally{Zt(t),br(r)}}function rs(e,t,r,n=r){e.addEventListener(t,()=>sn(r));const o=e.__on_r;o?e.__on_r=()=>{o(),n(!0)}:e.__on_r=()=>n(!0),Bi()}function Wi(e){Xe===null&&(De===null&&Ys(),Js()),Rr&&Ks()}function Vi(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function yr(e,t){var r=Xe;r!==null&&r.f&jt&&(e|=jt);var n={ctx:Wt,deps:null,nodes:null,f:e|Pt|Xt,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},o=n;if(e&_a)ba!==null?ba.push(n):vr(n);else if(t!==null){try{ha(n)}catch(l){throw Ot(n),l}o.deps===null&&o.teardown===null&&o.nodes===null&&o.first===o.last&&!(o.f&ka)&&(o=o.first,e&jr&&e&Sr&&o!==null&&(o.f|=Sr))}if(o!==null&&(o.parent=r,r!==null&&Vi(o,r),De!==null&&De.f&It&&!(e&Zr))){var s=De;(s.effects??(s.effects=[])).push(o)}return n}function Jn(){return De!==null&&!nr}function ln(e){const t=yr(ga,null);return wt(t,Tt),t.teardown=e,t}function Ft(e){Wi();var t=Xe.f,r=!De&&(t&sr)!==0&&(t&xa)===0;if(r){var n=Wt;(n.e??(n.e=[])).push(e)}else return as(e)}function as(e){return yr(_a|Ws,e)}function qi(e){Lr.ensure();const t=yr(Zr|ka,e);return(r={})=>new Promise(n=>{r.outro?Kr(t,()=>{Ot(t),n(void 0)}):(Ot(t),n(void 0))})}function dn(e){return yr(_a,e)}function Gi(e){return yr(Wn|ka,e)}function Yn(e,t=0){return yr(ga|t,e)}function M(e,t=[],r=[],n=[]){qo(n,t,r,o=>{yr(ga,()=>e(...o.map(a)))})}function wa(e,t=0){var r=yr(jr|t,e);return r}function ns(e,t=0){var r=yr(an|t,e);return r}function Bt(e){return yr(sr|ka,e)}function os(e){var t=e.teardown;if(t!==null){const r=Rr,n=De;fo(!0),Zt(null);try{t.call(null)}finally{fo(r),Zt(n)}}}function Xn(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const o=r.ac;o!==null&&sn(()=>{o.abort(Hr)});var n=r.next;r.f&Zr?r.parent=null:Ot(r,t),r=n}}function Ki(e){for(var t=e.first;t!==null;){var r=t.next;t.f&sr||Ot(t),t=r}}function Ot(e,t=!0){var r=!1;(t||e.f&Bs)&&e.nodes!==null&&e.nodes.end!==null&&(ss(e.nodes.start,e.nodes.end),r=!0),Xn(e,t&&!r),La(e,0),wt(e,gr);var n=e.nodes&&e.nodes.t;if(n!==null)for(const s of n)s.stop();os(e);var o=e.parent;o!==null&&o.first!==null&&is(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function ss(e,t){for(;e!==null;){var r=e===t?null:za(e);e.remove(),e=r}}function is(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function Kr(e,t,r=!0){var n=[];ls(e,n,!0);var o=()=>{r&&Ot(e),t&&t()},s=n.length;if(s>0){var l=()=>--s||o();for(var d of n)d.out(l)}else o()}function ls(e,t,r){if(!(e.f&jt)){e.f^=jt;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var o=e.first;o!==null;){var s=o.next,l=(o.f&Sr)!==0||(o.f&sr)!==0&&(e.f&jr)!==0;ls(o,t,l?r:!1),o=s}}}function Qn(e){ds(e,!0)}function ds(e,t){if(e.f&jt){e.f^=jt;for(var r=e.first;r!==null;){var n=r.next,o=(r.f&Sr)!==0||(r.f&sr)!==0;ds(r,o?t:!1),r=n}var s=e.nodes&&e.nodes.t;if(s!==null)for(const l of s)(l.is_global||t)&&l.in()}}function Zn(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var o=r===n?null:za(r);t.append(r),r=o}}let Ja=!1,Rr=!1;function fo(e){Rr=e}let De=null,nr=!1;function Zt(e){De=e}let Xe=null;function br(e){Xe=e}let Qt=null;function cs(e){De!==null&&(Qt===null?Qt=[e]:Qt.push(e))}let Ut=null,qt=0,Jt=null;function Ji(e){Jt=e}let us=1,zr=0,Jr=zr;function vo(e){Jr=e}function fs(){return++us}function Ua(e){var t=e.f;if(t&Pt)return!0;if(t&It&&(e.f&=~Yr),t&or){for(var r=e.deps,n=r.length,o=0;o<n;o++){var s=r[o];if(Ua(s)&&Ko(s),s.wv>e.wv)return!0}t&Xt&&Nt===null&&wt(e,Tt)}return!1}function vs(e,t,r=!0){var n=e.reactions;if(n!==null&&!(Qt!==null&&va.call(Qt,e)))for(var o=0;o<n.length;o++){var s=n[o];s.f&It?vs(s,t,!1):t===s&&(r?wt(s,Pt):s.f&Tt&&wt(s,or),vr(s))}}function gs(e){var j;var t=Ut,r=qt,n=Jt,o=De,s=Qt,l=Wt,d=nr,c=Jr,f=e.f;Ut=null,qt=0,Jt=null,De=f&(sr|Zr)?null:e,Qt=null,pa(e.ctx),nr=!1,Jr=++zr,e.ac!==null&&(sn(()=>{e.ac.abort(Hr)}),e.ac=null);try{e.f|=An;var h=e.fn,k=h();e.f|=xa;var w=e.deps,L=Me==null?void 0:Me.is_fork;if(Ut!==null){var T;if(L||La(e,qt),w!==null&&qt>0)for(w.length=qt+Ut.length,T=0;T<Ut.length;T++)w[qt+T]=Ut[T];else e.deps=w=Ut;if(Jn()&&e.f&Xt)for(T=qt;T<w.length;T++)((j=w[T]).reactions??(j.reactions=[])).push(e)}else!L&&w!==null&&qt<w.length&&(La(e,qt),w.length=qt);if(Ro()&&Jt!==null&&!nr&&w!==null&&!(e.f&(It|or|Pt)))for(T=0;T<Jt.length;T++)vs(Jt[T],e);if(o!==null&&o!==e){if(zr++,o.deps!==null)for(let N=0;N<r;N+=1)o.deps[N].rv=zr;if(t!==null)for(const N of t)N.rv=zr;Jt!==null&&(n===null?n=Jt:n.push(...Jt))}return e.f&Or&&(e.f^=Or),k}catch(N){return Ho(N)}finally{e.f^=An,Ut=t,qt=r,Jt=n,De=o,Qt=s,pa(l),nr=d,Jr=c}}function Yi(e,t){let r=t.reactions;if(r!==null){var n=Rs.call(r,e);if(n!==-1){var o=r.length-1;o===0?r=t.reactions=null:(r[n]=r[o],r.pop())}}if(r===null&&t.f&It&&(Ut===null||!va.call(Ut,t))){var s=t;s.f&Xt&&(s.f^=Xt,s.f&=~Yr),qn(s),Fi(s),La(s,0)}}function La(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)Yi(e,r[n])}function ha(e){var t=e.f;if(!(t&gr)){wt(e,Tt);var r=Xe,n=Ja;Xe=e,Ja=!0;try{t&(jr|an)?Ki(e):Xn(e),os(e);var o=gs(e);e.teardown=typeof o=="function"?o:null,e.wv=us;var s;wn&&hi&&e.f&Pt&&e.deps}finally{Ja=n,Xe=r}}}async function ps(){await Promise.resolve(),ki()}function a(e){var t=e.f,r=(t&It)!==0;if(De!==null&&!nr){var n=Xe!==null&&(Xe.f&gr)!==0;if(!n&&(Qt===null||!va.call(Qt,e))){var o=De.deps;if(De.f&An)e.rv<zr&&(e.rv=zr,Ut===null&&o!==null&&o[qt]===e?qt++:Ut===null?Ut=[e]:Ut.push(e));else{(De.deps??(De.deps=[])).push(e);var s=e.reactions;s===null?e.reactions=[De]:va.call(s,De)||s.push(De)}}}if(Rr&&Fr.has(e))return Fr.get(e);if(r){var l=e;if(Rr){var d=l.v;return(!(l.f&Tt)&&l.reactions!==null||ys(l))&&(d=Gn(l)),Fr.set(l,d),d}var c=(l.f&Xt)===0&&!nr&&De!==null&&(Ja||(De.f&Xt)!==0),f=(l.f&xa)===0;Ua(l)&&(c&&(l.f|=Xt),Ko(l)),c&&!f&&(Jo(l),bs(l))}if(Nt!=null&&Nt.has(e))return Nt.get(e);if(e.f&Or)throw e.v;return e.v}function bs(e){if(e.f|=Xt,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&It&&!(t.f&Xt)&&(Jo(t),bs(t))}function ys(e){if(e.v===Ct)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(Fr.has(t)||t.f&It&&ys(t))return!0;return!1}function Sa(e){var t=nr;try{return nr=!0,e()}finally{nr=t}}function Xi(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Qi=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function Zi(e){return Qi.includes(e)}const el={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function tl(e){return e=e.toLowerCase(),el[e]??e}const rl=["touchstart","touchmove"];function al(e){return rl.includes(e)}const Ur=Symbol("events"),hs=new Set,In=new Set;function ms(e,t,r,n={}){function o(s){if(n.capture||Ln.call(t,s),!s.cancelBubble)return sn(()=>r==null?void 0:r.call(this,s))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?pr(()=>{t.addEventListener(e,o,n)}):t.addEventListener(e,o,n),o}function xr(e,t,r,n,o){var s={capture:n,passive:o},l=ms(e,t,r,s);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&ln(()=>{t.removeEventListener(e,l,s)})}function G(e,t,r){(t[Ur]??(t[Ur]={}))[e]=r}function ir(e){for(var t=0;t<e.length;t++)hs.add(e[t]);for(var r of In)r(e)}let go=null;function Ln(e){var N,I;var t=this,r=t.ownerDocument,n=e.type,o=((N=e.composedPath)==null?void 0:N.call(e))||[],s=o[0]||e.target;go=e;var l=0,d=go===e&&e[Ur];if(d){var c=o.indexOf(d);if(c!==-1&&(t===document||t===window)){e[Ur]=t;return}var f=o.indexOf(t);if(f===-1)return;c<=f&&(l=c)}if(s=o[l]||e.target,s!==t){js(e,"currentTarget",{configurable:!0,get(){return s||r}});var h=De,k=Xe;Zt(null),br(null);try{for(var w,L=[];s!==null;){var T=s.assignedSlot||s.parentNode||s.host||null;try{var j=(I=s[Ur])==null?void 0:I[n];j!=null&&(!s.disabled||e.target===s)&&j.call(s,e)}catch(X){w?L.push(X):w=X}if(e.cancelBubble||T===t||T===null)break;s=T}if(w){for(let X of L)queueMicrotask(()=>{throw X});throw w}}finally{e[Ur]=t,delete e.currentTarget,Zt(h),br(k)}}}var Eo;const pn=((Eo=globalThis==null?void 0:globalThis.window)==null?void 0:Eo.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function nl(e){return(pn==null?void 0:pn.createHTML(e))??e}function _s(e){var t=Kn("template");return t.innerHTML=nl(e.replaceAll("<!>","<!---->")),t.content}function ma(e,t){var r=Xe;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function x(e,t){var r=(t&ui)!==0,n=(t&fi)!==0,o,s=!e.startsWith("<!>");return()=>{o===void 0&&(o=_s(s?e:"<!>"+e),r||(o=wr(o)));var l=n||Qo?document.importNode(o,!0):o.cloneNode(!0);if(r){var d=wr(l),c=l.lastChild;ma(d,c)}else ma(l,l);return l}}function ol(e,t,r="svg"){var n=!e.startsWith("<!>"),o=`<${r}>${n?e:"<!>"+e}</${r}>`,s;return()=>{if(!s){var l=_s(o),d=wr(l);s=wr(d)}var c=s.cloneNode(!0);return ma(c,c),c}}function sl(e,t){return ol(e,t,"svg")}function He(){var e=document.createDocumentFragment(),t=document.createComment(""),r=kr();return e.append(t,r),ma(t,r),e}function v(e,t){e!==null&&e.before(t)}function b(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function il(e,t){return ll(e,t)}const Va=new Map;function ll(e,{target:t,anchor:r,props:n={},events:o,context:s,intro:l=!0,transformError:d}){Di();var c=void 0,f=qi(()=>{var h=r??t.appendChild(kr());Ei(h,{pending:()=>{}},L=>{Pe({});var T=Wt;s&&(T.c=s),o&&(n.$$events=o),c=e(L,n)||{},Oe()},d);var k=new Set,w=L=>{for(var T=0;T<L.length;T++){var j=L[T];if(!k.has(j)){k.add(j);var N=al(j);for(const q of[t,document]){var I=Va.get(q);I===void 0&&(I=new Map,Va.set(q,I));var X=I.get(j);X===void 0?(q.addEventListener(j,Ln,{passive:N}),I.set(j,1)):I.set(j,X+1)}}}};return w(rn(hs)),In.add(w),()=>{var N;for(var L of k)for(const I of[t,document]){var T=Va.get(I),j=T.get(L);--j==0?(I.removeEventListener(L,Ln),T.delete(L),T.size===0&&Va.delete(I)):T.set(L,j)}In.delete(w),h!==r&&((N=h.parentNode)==null||N.removeChild(h))}});return dl.set(c,f),c}let dl=new WeakMap;var ar,ur,Kt,Gr,Ha,Da,tn;class cn{constructor(t,r=!0){er(this,"anchor");We(this,ar,new Map);We(this,ur,new Map);We(this,Kt,new Map);We(this,Gr,new Set);We(this,Ha,!0);We(this,Da,t=>{if(E(this,ar).has(t)){var r=E(this,ar).get(t),n=E(this,ur).get(r);if(n)Qn(n),E(this,Gr).delete(r);else{var o=E(this,Kt).get(r);o&&!(o.effect.f&jt)&&(E(this,ur).set(r,o.effect),E(this,Kt).delete(r),o.fragment.lastChild.remove(),this.anchor.before(o.fragment),n=o.effect)}for(const[s,l]of E(this,ar)){if(E(this,ar).delete(s),s===t)break;const d=E(this,Kt).get(l);d&&(Ot(d.effect),E(this,Kt).delete(l))}for(const[s,l]of E(this,ur)){if(s===r||E(this,Gr).has(s)||l.f&jt)continue;const d=()=>{if(Array.from(E(this,ar).values()).includes(s)){var f=document.createDocumentFragment();Zn(l,f),f.append(kr()),E(this,Kt).set(s,{effect:l,fragment:f})}else Ot(l);E(this,Gr).delete(s),E(this,ur).delete(s)};E(this,Ha)||!n?(E(this,Gr).add(s),Kr(l,d,!1)):d()}}});We(this,tn,t=>{E(this,ar).delete(t);const r=Array.from(E(this,ar).values());for(const[n,o]of E(this,Kt))r.includes(n)||(Ot(o.effect),E(this,Kt).delete(n))});this.anchor=t,Le(this,Ha,r)}ensure(t,r){var n=Me,o=ts();if(r&&!E(this,ur).has(t)&&!E(this,Kt).has(t))if(o){var s=document.createDocumentFragment(),l=kr();s.append(l),E(this,Kt).set(t,{effect:Bt(()=>r(l)),fragment:s})}else E(this,ur).set(t,Bt(()=>r(this.anchor)));if(E(this,ar).set(n,t),o){for(const[d,c]of E(this,ur))d===t?n.unskip_effect(c):n.skip_effect(c);for(const[d,c]of E(this,Kt))d===t?n.unskip_effect(c.effect):n.skip_effect(c.effect);n.oncommit(E(this,Da)),n.ondiscard(E(this,tn))}else E(this,Da).call(this,n)}}ar=new WeakMap,ur=new WeakMap,Kt=new WeakMap,Gr=new WeakMap,Ha=new WeakMap,Da=new WeakMap,tn=new WeakMap;function z(e,t,r=!1){var n=new cn(e),o=r?Sr:0;function s(l,d){n.ensure(l,d)}wa(()=>{var l=!1;t((d,c=0)=>{l=!0,s(c,d)}),l||s(-1,null)},o)}function rt(e,t){return t}function cl(e,t,r){for(var n=[],o=t.length,s,l=t.length,d=0;d<o;d++){let k=t[d];Kr(k,()=>{if(s){if(s.pending.delete(k),s.done.add(k),s.pending.size===0){var w=e.outrogroups;Fn(e,rn(s.done)),w.delete(s),w.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var c=n.length===0&&r!==null;if(c){var f=r,h=f.parentNode;zi(h),h.append(f),e.items.clear()}Fn(e,t,!c)}else s={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(s)}function Fn(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var o=0;o<t.length;o++){var s=t[o];if(n!=null&&n.has(s)){s.f|=fr;const l=document.createDocumentFragment();Zn(s,l)}else Ot(t[o],r)}}var po;function Qe(e,t,r,n,o,s=null){var l=e,d=new Map,c=(t&Po)!==0;if(c){var f=e;l=f.appendChild(kr())}var h=null,k=Go(()=>{var q=r();return Bn(q)?q:q==null?[]:rn(q)}),w,L=new Map,T=!0;function j(q){X.effect.f&gr||(X.pending.delete(q),X.fallback=h,ul(X,w,l,t,n),h!==null&&(w.length===0?h.f&fr?(h.f^=fr,Ta(h,null,l)):Qn(h):Kr(h,()=>{h=null})))}function N(q){X.pending.delete(q)}var I=wa(()=>{w=a(k);for(var q=w.length,A=new Set,m=Me,$=ts(),P=0;P<q;P+=1){var U=w[P],Se=n(U,P),me=T?null:d.get(Se);me?(me.v&&ya(me.v,U),me.i&&ya(me.i,P),$&&m.unskip_effect(me.e)):(me=fl(d,T?l:po??(po=kr()),U,Se,P,o,t,r),T||(me.e.f|=fr),d.set(Se,me)),A.add(Se)}if(q===0&&s&&!h&&(T?h=Bt(()=>s(l)):(h=Bt(()=>s(po??(po=kr()))),h.f|=fr)),q>A.size&&Gs(),!T)if(L.set(m,A),$){for(const[Fe,Ge]of d)A.has(Fe)||m.skip_effect(Ge.e);m.oncommit(j),m.ondiscard(N)}else j(m);a(k)}),X={effect:I,items:d,pending:L,outrogroups:null,fallback:h};T=!1}function $a(e){for(;e!==null&&!(e.f&sr);)e=e.next;return e}function ul(e,t,r,n,o){var me,Fe,Ge,B,K,J,ee,Ie,D;var s=(n&oi)!==0,l=t.length,d=e.items,c=$a(e.effect.first),f,h=null,k,w=[],L=[],T,j,N,I;if(s)for(I=0;I<l;I+=1)T=t[I],j=o(T,I),N=d.get(j).e,N.f&fr||((Fe=(me=N.nodes)==null?void 0:me.a)==null||Fe.measure(),(k??(k=new Set)).add(N));for(I=0;I<l;I+=1){if(T=t[I],j=o(T,I),N=d.get(j).e,e.outrogroups!==null)for(const W of e.outrogroups)W.pending.delete(N),W.done.delete(N);if(N.f&fr)if(N.f^=fr,N===c)Ta(N,null,r);else{var X=h?h.next:c;N===e.effect.last&&(e.effect.last=N.prev),N.prev&&(N.prev.next=N.next),N.next&&(N.next.prev=N.prev),Er(e,h,N),Er(e,N,X),Ta(N,X,r),h=N,w=[],L=[],c=$a(h.next);continue}if(N.f&jt&&(Qn(N),s&&((B=(Ge=N.nodes)==null?void 0:Ge.a)==null||B.unfix(),(k??(k=new Set)).delete(N))),N!==c){if(f!==void 0&&f.has(N)){if(w.length<L.length){var q=L[0],A;h=q.prev;var m=w[0],$=w[w.length-1];for(A=0;A<w.length;A+=1)Ta(w[A],q,r);for(A=0;A<L.length;A+=1)f.delete(L[A]);Er(e,m.prev,$.next),Er(e,h,m),Er(e,$,q),c=q,h=$,I-=1,w=[],L=[]}else f.delete(N),Ta(N,c,r),Er(e,N.prev,N.next),Er(e,N,h===null?e.effect.first:h.next),Er(e,h,N),h=N;continue}for(w=[],L=[];c!==null&&c!==N;)(f??(f=new Set)).add(c),L.push(c),c=$a(c.next);if(c===null)continue}N.f&fr||w.push(N),h=N,c=$a(N.next)}if(e.outrogroups!==null){for(const W of e.outrogroups)W.pending.size===0&&(Fn(e,rn(W.done)),(K=e.outrogroups)==null||K.delete(W));e.outrogroups.size===0&&(e.outrogroups=null)}if(c!==null||f!==void 0){var P=[];if(f!==void 0)for(N of f)N.f&jt||P.push(N);for(;c!==null;)!(c.f&jt)&&c!==e.fallback&&P.push(c),c=$a(c.next);var U=P.length;if(U>0){var Se=n&Po&&l===0?r:null;if(s){for(I=0;I<U;I+=1)(ee=(J=P[I].nodes)==null?void 0:J.a)==null||ee.measure();for(I=0;I<U;I+=1)(D=(Ie=P[I].nodes)==null?void 0:Ie.a)==null||D.fix()}cl(e,P,Se)}}s&&pr(()=>{var W,be;if(k!==void 0)for(N of k)(be=(W=N.nodes)==null?void 0:W.a)==null||be.apply()})}function fl(e,t,r,n,o,s,l,d){var c=l&ai?l&si?Xr(r):Ri(r,!1,!1):null,f=l&ni?Xr(o):null;return{v:c,i:f,e:Bt(()=>(s(t,c??r,f??o,d),()=>{e.delete(n)}))}}function Ta(e,t,r){if(e.nodes)for(var n=e.nodes.start,o=e.nodes.end,s=t&&!(t.f&fr)?t.nodes.start:r;n!==null;){var l=za(n);if(s.before(n),n===o)return;n=l}}function Er(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function vl(e,t,r=!1,n=!1,o=!1){var s=e,l="";M(()=>{var d=Xe;if(l!==(l=t()??"")&&(d.nodes!==null&&(ss(d.nodes.start,d.nodes.end),d.nodes=null),l!=="")){var c=r?Io:n?vi:void 0,f=Kn(r?"svg":n?"math":"template",c);f.innerHTML=l;var h=r||n?f:f.content;if(ma(wr(h),h.lastChild),r||n)for(;wr(h);)s.before(wr(h));else s.before(h)}})}function ft(e,t,...r){var n=new cn(e);wa(()=>{const o=t()??null;n.ensure(o,o&&(s=>o(s,...r)))},Sr)}function gl(e,t,r){var n=new cn(e);wa(()=>{var o=t()??null;n.ensure(o,o&&(s=>r(s,o)))},Sr)}function pl(e,t,r,n,o,s){var l=null,d=e,c=new cn(d,!1);wa(()=>{const f=t()||null;var h=Io;if(f===null){c.ensure(null,null);return}return c.ensure(f,k=>{if(f){if(l=Kn(f,h),ma(l,l),n){var w=l.appendChild(kr());n(l,w)}Xe.nodes.end=l,k.before(l)}}),()=>{}},Sr),ln(()=>{})}function bl(e,t){var r=void 0,n;ns(()=>{r!==(r=t())&&(n&&(Ot(n),n=null),r&&(n=Bt(()=>{dn(()=>r(e))})))})}function xs(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var o=e.length;for(t=0;t<o;t++)e[t]&&(r=xs(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function yl(){for(var e,t,r=0,n="",o=arguments.length;r<o;r++)(e=arguments[r])&&(t=xs(e))&&(n&&(n+=" "),n+=t);return n}function ks(e){return typeof e=="object"?yl(e):e??""}const bo=[...` 	
\r\f \v\uFEFF`];function hl(e,t,r){var n=e==null?"":""+e;if(r){for(var o of Object.keys(r))if(r[o])n=n?n+" "+o:o;else if(n.length)for(var s=o.length,l=0;(l=n.indexOf(o,l))>=0;){var d=l+s;(l===0||bo.includes(n[l-1]))&&(d===n.length||bo.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function yo(e,t=!1){var r=t?" !important;":";",n="";for(var o of Object.keys(e)){var s=e[o];s!=null&&s!==""&&(n+=" "+o+": "+s+r)}return n}function bn(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function ml(e,t){if(t){var r="",n,o;if(Array.isArray(t)?(n=t[0],o=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var s=!1,l=0,d=!1,c=[];n&&c.push(...Object.keys(n).map(bn)),o&&c.push(...Object.keys(o).map(bn));var f=0,h=-1;const j=e.length;for(var k=0;k<j;k++){var w=e[k];if(d?w==="/"&&e[k-1]==="*"&&(d=!1):s?s===w&&(s=!1):w==="/"&&e[k+1]==="*"?d=!0:w==='"'||w==="'"?s=w:w==="("?l++:w===")"&&l--,!d&&s===!1&&l===0){if(w===":"&&h===-1)h=k;else if(w===";"||k===j-1){if(h!==-1){var L=bn(e.substring(f,h).trim());if(!c.includes(L)){w!==";"&&k++;var T=e.substring(f,k).trim();r+=" "+T+";"}}f=k+1,h=-1}}}}return n&&(r+=yo(n)),o&&(r+=yo(o,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function Ye(e,t,r,n,o,s){var l=e.__className;if(l!==r||l===void 0){var d=hl(r,n,s);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(s&&o!==s)for(var c in s){var f=!!s[c];(o==null||f!==!!o[c])&&e.classList.toggle(c,f)}return s}function yn(e,t={},r,n){for(var o in r){var s=r[o];t[o]!==s&&(r[o]==null?e.style.removeProperty(o):e.style.setProperty(o,s,n))}}function _l(e,t,r,n){var o=e.__style;if(o!==t){var s=ml(t,n);s==null?e.removeAttribute("style"):e.style.cssText=s,e.__style=t}else n&&(Array.isArray(n)?(yn(e,r==null?void 0:r[0],n[0]),yn(e,r==null?void 0:r[1],n[1],"important")):yn(e,r,n));return n}function Fa(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Bn(t))return pi();for(var n of e.options)n.selected=t.includes(Ia(n));return}for(n of e.options){var o=Ia(n);if(Hi(o,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function eo(e){var t=new MutationObserver(()=>{Fa(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),ln(()=>{t.disconnect()})}function Rn(e,t,r=t){var n=new WeakSet,o=!0;rs(e,"change",s=>{var l=s?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),Ia);else{var c=e.querySelector(l)??e.querySelector("option:not([disabled])");d=c&&Ia(c)}r(d),Me!==null&&n.add(Me)}),dn(()=>{var s=t();if(e===document.activeElement){var l=Ya??Me;if(n.has(l))return}if(Fa(e,s,o),o&&s===void 0){var d=e.querySelector(":checked");d!==null&&(s=Ia(d),r(s))}e.__value=s,o=!1}),eo(e)}function Ia(e){return"__value"in e?e.__value:e.value}const Ca=Symbol("class"),Ma=Symbol("style"),ws=Symbol("is custom element"),Ss=Symbol("is html"),xl=Vn?"option":"OPTION",kl=Vn?"select":"SELECT",wl=Vn?"progress":"PROGRESS";function hr(e,t){var r=to(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==wl)||(e.value=t??"")}function Sl(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function ut(e,t,r,n){var o=to(e);o[t]!==(o[t]=r)&&(t==="loading"&&(e[Vs]=r),r==null?e.removeAttribute(t):typeof r!="string"&&As(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function Al(e,t,r,n,o=!1,s=!1){var l=to(e),d=l[ws],c=!l[Ss],f=t||{},h=e.nodeName===xl;for(var k in t)k in r||(r[k]=null);r.class?r.class=ks(r.class):r[Ca]&&(r.class=null),r[Ma]&&(r.style??(r.style=null));var w=As(e);for(const A in r){let m=r[A];if(h&&A==="value"&&m==null){e.value=e.__value="",f[A]=m;continue}if(A==="class"){var L=e.namespaceURI==="http://www.w3.org/1999/xhtml";Ye(e,L,m,n,t==null?void 0:t[Ca],r[Ca]),f[A]=m,f[Ca]=r[Ca];continue}if(A==="style"){_l(e,m,t==null?void 0:t[Ma],r[Ma]),f[A]=m,f[Ma]=r[Ma];continue}var T=f[A];if(!(m===T&&!(m===void 0&&e.hasAttribute(A)))){f[A]=m;var j=A[0]+A[1];if(j!=="$$")if(j==="on"){const $={},P="$$"+A;let U=A.slice(2);var N=Zi(U);if(Xi(U)&&(U=U.slice(0,-7),$.capture=!0),!N&&T){if(m!=null)continue;e.removeEventListener(U,f[P],$),f[P]=null}if(N)G(U,e,m),ir([U]);else if(m!=null){let Se=function(me){f[A].call(this,me)};var q=Se;f[P]=ms(U,e,Se,$)}}else if(A==="style")ut(e,A,m);else if(A==="autofocus")Ui(e,!!m);else if(!d&&(A==="__value"||A==="value"&&m!=null))e.value=e.__value=m;else if(A==="selected"&&h)Sl(e,m);else{var I=A;c||(I=tl(I));var X=I==="defaultValue"||I==="defaultChecked";if(m==null&&!d&&!X)if(l[A]=null,I==="value"||I==="checked"){let $=e;const P=t===void 0;if(I==="value"){let U=$.defaultValue;$.removeAttribute(I),$.defaultValue=U,$.value=$.__value=P?U:null}else{let U=$.defaultChecked;$.removeAttribute(I),$.defaultChecked=U,$.checked=P?U:!1}}else e.removeAttribute(A);else X||w.includes(I)&&(d||typeof m!="string")?(e[I]=m,I in l&&(l[I]=Ct)):typeof m!="function"&&ut(e,I,m)}}}return f}function ho(e,t,r=[],n=[],o=[],s,l=!1,d=!1){qo(o,r,n,c=>{var f=void 0,h={},k=e.nodeName===kl,w=!1;if(ns(()=>{var T=t(...c.map(a)),j=Al(e,f,T,s,l,d);w&&k&&"value"in T&&Fa(e,T.value);for(let I of Object.getOwnPropertySymbols(h))T[I]||Ot(h[I]);for(let I of Object.getOwnPropertySymbols(T)){var N=T[I];I.description===gi&&(!f||N!==f[I])&&(h[I]&&Ot(h[I]),h[I]=Bt(()=>bl(e,()=>N))),j[I]=N}f=j}),k){var L=e;dn(()=>{Fa(L,f.value,!0),eo(L)})}w=!0})}function to(e){return e.__attributes??(e.__attributes={[ws]:e.nodeName.includes("-"),[Ss]:e.namespaceURI===Oo})}var mo=new Map;function As(e){var t=e.getAttribute("is")||e.nodeName,r=mo.get(t);if(r)return r;mo.set(t,r=[]);for(var n,o=e,s=Element.prototype;s!==o;){n=Hs(o);for(var l in n)n[l].set&&r.push(l);o=Co(o)}return r}function Br(e,t,r=t){var n=new WeakSet;rs(e,"input",async o=>{var s=o?e.defaultValue:e.value;if(s=hn(e)?mn(s):s,r(s),Me!==null&&n.add(Me),await ps(),s!==(s=t())){var l=e.selectionStart,d=e.selectionEnd,c=e.value.length;if(e.value=s??"",d!==null){var f=e.value.length;l===d&&d===c&&f>c?(e.selectionStart=f,e.selectionEnd=f):(e.selectionStart=l,e.selectionEnd=Math.min(d,f))}}}),Sa(t)==null&&e.value&&(r(hn(e)?mn(e.value):e.value),Me!==null&&n.add(Me)),Yn(()=>{var o=t();if(e===document.activeElement){var s=Ya??Me;if(n.has(s))return}hn(e)&&o===mn(e.value)||e.type==="date"&&!o&&!e.value||o!==e.value&&(e.value=o??"")})}function hn(e){var t=e.type;return t==="number"||t==="range"}function mn(e){return e===""?null:+e}function _o(e,t){return e===t||(e==null?void 0:e[Ir])===t}function jn(e={},t,r,n){return dn(()=>{var o,s;return Yn(()=>{o=s,s=[],Sa(()=>{e!==r(...s)&&(t(e,...s),o&&_o(r(...o),e)&&t(null,...o))})}),()=>{pr(()=>{s&&_o(r(...s),e)&&t(null,...s)})}}),e}let qa=!1;function El(e){var t=qa;try{return qa=!1,[e(),qa]}finally{qa=t}}const $l={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function vt(e,t,r){return new Proxy({props:e,exclude:t},$l)}const Cl={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ea(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let o=e.props[n];Ea(o)&&(o=o());const s=Pr(o,t);if(s&&s.set)return s.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ea(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const o=Pr(n,t);return o&&!o.configurable&&(o.configurable=!0),o}}},has(e,t){if(t===Ir||t===No)return!1;for(let r of e.props)if(Ea(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Ea(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function pt(...e){return new Proxy({props:e},Cl)}function aa(e,t,r,n){var X;var o=(r&di)!==0,s=(r&ci)!==0,l=n,d=!0,c=()=>(d&&(d=!1,l=s?Sa(n):n),l),f;if(o){var h=Ir in e||No in e;f=((X=Pr(e,t))==null?void 0:X.set)??(h&&t in e?q=>e[t]=q:void 0)}var k,w=!1;o?[k,w]=El(()=>e[t]):k=e[t],k===void 0&&n!==void 0&&(k=c(),f&&(Qs(),f(k)));var L;if(L=()=>{var q=e[t];return q===void 0?c():(d=!0,q)},!(r&li))return L;if(f){var T=e.$$legacy;return function(q,A){return arguments.length>0?((!A||T||w)&&f(A?L():q),q):L()}}var j=!1,N=(r&ii?on:Go)(()=>(j=!1,L()));o&&a(N);var I=Xe;return function(q,A){if(arguments.length>0){const m=A?a(N):o?mt(q):q;return p(N,m),j=!0,l!==void 0&&(l=m),q}return Rr&&j||I.f&gr?N.v:a(N)}}function Ml(e){Wt===null&&To(),Ft(()=>{const t=Sa(e);if(typeof t=="function")return t})}function Nl(e){Wt===null&&To(),Ml(()=>()=>Sa(e))}const Tl="5";var $o;typeof window<"u"&&(($o=window.__svelte??(window.__svelte={})).v??($o.v=new Set)).add(Tl);const ro="prx-console-token",Pl=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}];function Ra(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(ro))==null?void 0:e.trim())??""}function Ol(e){typeof window>"u"||window.localStorage.setItem(ro,e.trim())}function Es(){typeof window>"u"||window.localStorage.removeItem(ro)}function $s(){return typeof window>"u"?"/":window.location.pathname||"/"}function $r(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function Il(e){if(typeof window>"u")return()=>{};const t=()=>{e($s())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}const _n="".trim(),Xa=_n.endsWith("/")?_n.slice(0,-1):_n;class xo extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function Ll(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Fl(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function Mt(e,t={}){const r=Ra(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const o=await fetch(`${Xa}${e}`,{...t,headers:n}),s=await Ll(o);if(o.status===401)throw Es(),$r("/",!0),new xo(401,"Unauthorized");if(!o.ok)throw new xo(o.status,Fl(s,o.status));return s}const St={getStatus:()=>Mt("/api/status"),getSessions:()=>Mt("/api/sessions"),getSessionMessages:e=>Mt(`/api/sessions/${encodeURIComponent(e)}/messages`),sendMessage:(e,t)=>Mt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return St.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const o of r)n.append("files",o);return Mt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e}),r=Ra();return r&&t.set("token",r),`${Xa}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>Mt("/api/channels/status"),getConfig:()=>Mt("/api/config"),saveConfig:e=>Mt("/api/config",{method:"POST",body:JSON.stringify(e)}),getHooks:()=>Mt("/api/hooks"),getMcpServers:()=>Mt("/api/mcp/servers"),getSkills:()=>Mt("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),Mt(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>Mt("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>Mt(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>Mt(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>Mt("/api/plugins"),reloadPlugin:e=>Mt(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})},Qa={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function Hn(e){return String(e).replace(/_/g," ").replace(/\b\w/g,t=>t.toUpperCase())}function Rl(){const e=new Set;for(const t of Object.values(Qa))for(const r of Object.keys(t.fields))e.add(r.split(".")[0]);return e}const Cs=Rl();function Dn(e){const t=Object.entries(Qa).map(([n,o])=>({groupKey:n,label:o.label,dynamic:!1}));if(!e||typeof e!="object")return t;const r=Object.keys(e).filter(n=>!Cs.has(n)).sort().map(n=>({groupKey:n,label:Hn(n),dynamic:!0}));return[...t,...r]}function Za(e){return`config-section-${e}`}function Ms(e){if(typeof document>"u"||typeof window>"u")return;const t=document.getElementById(Za(e));t instanceof HTMLDetailsElement&&(t.open=!0),t&&t.scrollIntoView({behavior:"smooth",block:"start"});const r=`#${Za(e)}`;window.location.hash!==r&&(window.location.hash=r)}const jl={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",notFound:"Not found",backToOverview:"Back to Overview"},Hl={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Dl={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},zl={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},Ul={title:"Sessions",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No active sessions"},Bl={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Wl={title:"Channels",type:"Type",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI"}},Vl={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings"},ql={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},Gl={title:"Hooks",loading:"Loading hooks...",noHooks:"No hooks configured.",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",edit:"Edit",delete:"Delete",save:"Save",cancel:"Cancel"},Kl={title:"MCP Servers",loading:"Loading MCP servers...",noServers:"No MCP servers configured.",connected:"Connected",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},Jl={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},Yl={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},Xl={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},Ql={app:jl,nav:Hl,common:Dl,overview:zl,sessions:Ul,chat:Bl,channels:Wl,config:Vl,logs:ql,hooks:Gl,mcp:Kl,skills:Jl,plugins:Yl,login:Xl},Zl={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",notFound:"页面未找到",backToOverview:"返回概览"},ed={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},td={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},rd={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},ad={title:"会话",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"当前没有活跃会话"},nd={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},od={title:"通道",type:"类型",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行"}},sd={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项"},id={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},ld={title:"Hooks",loading:"正在加载 Hooks...",noHooks:"尚未配置任何 Hook。",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",edit:"编辑",delete:"删除",save:"保存",cancel:"取消"},dd={title:"MCP 服务",loading:"正在加载 MCP 服务...",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},cd={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},ud={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},fd={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},vd={app:Zl,nav:ed,common:td,overview:rd,sessions:ad,chat:nd,channels:od,config:sd,logs:id,hooks:ld,mcp:dd,skills:cd,plugins:ud,login:fd},un="prx-console-lang",ja="en",xn={en:Ql,zh:vd};function zn(e){return typeof e!="string"||e.trim().length===0?ja:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function gd(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(un);if(t)return zn(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||ja;return zn(t)}return ja}function ko(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function Ns(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function pd(e){typeof window<"u"&&window.localStorage.setItem(un,e)}const Qr=mt({lang:gd()});Ns(Qr.lang);function Ts(e){const t=zn(e);Qr.lang!==t&&(Qr.lang=t,pd(t),Ns(t))}function na(){Ts(Qr.lang==="en"?"zh":"en")}function bd(){if(typeof window>"u")return;const e=window.localStorage.getItem(un);e&&Ts(e)}function _(e,t={}){const r=xn[Qr.lang]??xn[ja];let n=ko(r,e);if(typeof n!="string"&&(n=ko(xn[ja],e)),typeof n!="string")return e;for(const[o,s]of Object.entries(t))n=n.replaceAll(`{${o}}`,String(s));return n}/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const yd={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 * 
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 * 
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 * 
 * ---
 * 
 * The MIT License (MIT) (for portions derived from Feather)
 * 
 * Copyright (c) 2013-2026 Cole Bemis
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 * 
 */const hd=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var md=sl("<svg><!><!></svg>");function bt(e,t){Pe(t,!0);const r=aa(t,"color",3,"currentColor"),n=aa(t,"size",3,24),o=aa(t,"strokeWidth",3,2),s=aa(t,"absoluteStrokeWidth",3,!1),l=aa(t,"iconNode",19,()=>[]),d=vt(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var c=md();ho(c,(k,w)=>({...yd,...k,...d,width:n(),height:n(),stroke:r(),"stroke-width":w,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!hd(d)&&{"aria-hidden":"true"},()=>s()?Number(o())*24/Number(n()):o()]);var f=i(c);Qe(f,17,l,rt,(k,w)=>{var L=te(()=>Na(a(w),2));let T=()=>a(L)[0],j=()=>a(L)[1];var N=He(),I=Ee(N);pl(I,T,!0,(X,q)=>{ho(X,()=>({...j()}))}),v(k,N)});var h=g(f);ft(h,()=>t.children??Ce),v(e,c),Oe()}function _d(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];bt(e,pt({name:"badge-check"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function wo(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];bt(e,pt({name:"blocks"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function xd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];bt(e,pt({name:"bot"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function kd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];bt(e,pt({name:"brain"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function wd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];bt(e,pt({name:"cable"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Sd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];bt(e,pt({name:"chart-column"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Ad(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];bt(e,pt({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Ed(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];bt(e,pt({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function $d(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];bt(e,pt({name:"clock"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Cd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m18 16 4-4-4-4"}],["path",{d:"m6 8-4 4 4 4"}],["path",{d:"m14.5 4-5 16"}]];bt(e,pt({name:"code-xml"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Md(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];bt(e,pt({name:"database"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Nd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];bt(e,pt({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Td(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];bt(e,pt({name:"git-branch"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Pd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];bt(e,pt({name:"globe"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Od(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];bt(e,pt({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Id(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];bt(e,pt({name:"loader"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Ld(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];bt(e,pt({name:"message-square"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Fd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];bt(e,pt({name:"moon"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Rd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];bt(e,pt({name:"paperclip"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Ps(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];bt(e,pt({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function jd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];bt(e,pt({name:"search"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Hd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];bt(e,pt({name:"settings"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Dd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];bt(e,pt({name:"shield"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function zd(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];bt(e,pt({name:"sun"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}function Ud(e,t){Pe(t,!0);/**
 * @license @lucide/svelte v0.577.0 - ISC
 *
 * ISC License
 *
 * Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 * ---
 *
 * The MIT License (MIT) (for portions derived from Feather)
 *
 * Copyright (c) 2013-2026 Cole Bemis
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 *
 */let r=vt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];bt(e,pt({name:"zap"},()=>r,{get iconNode(){return n},children:(o,s)=>{var l=He(),d=Ee(l);ft(d,()=>t.children??Ce),v(o,l)},$$slots:{default:!0}})),Oe()}var Bd=x('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Wd=x('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function Vd(e,t){Pe(t,!0);let r=R(""),n=R("");function o($){var U;$.preventDefault();const P=a(r).trim();if(!P){p(n,_("login.tokenRequired"),!0);return}Ol(P),p(n,""),(U=t.onLogin)==null||U.call(t,P)}var s=Wd(),l=i(s),d=i(l),c=i(d),f=i(c),h=g(c,2),k=i(h),w=g(d,2),L=i(w),T=g(w,2),j=i(T),N=i(j),I=g(j,2),X=g(I,2);{var q=$=>{var P=Bd(),U=i(P);M(()=>b(U,a(n))),v($,P)};z(X,$=>{a(n)&&$(q)})}var A=g(X,2),m=i(A);M(($,P,U,Se,me,Fe)=>{b(f,$),ut(h,"aria-label",P),b(k,Qr.lang==="zh"?"中文 / EN":"EN / 中文"),b(L,U),b(N,Se),ut(I,"placeholder",me),b(m,Fe)},[()=>_("login.title"),()=>_("app.language"),()=>_("login.hint"),()=>_("login.accessToken"),()=>_("login.placeholder"),()=>_("login.login")]),G("click",h,function(...$){na==null||na.apply(this,$)}),xr("submit",T,o),Br(I,()=>a(r),$=>p(r,$)),v(e,s),Oe()}ir(["click"]);function qd(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),o=Math.floor(e%60),s=[];return t>0&&s.push(`${t}d`),(r>0||s.length>0)&&s.push(`${r}h`),(n>0||s.length>0)&&s.push(`${n}m`),s.push(`${o}s`),s.join(" ")}var Gd=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Kd=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Jd=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Yd=x('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),Xd=x('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Qd=x('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),Zd=x('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),ec=x('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),tc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function rc(e,t){Pe(t,!0);let r=R(null),n=R(!0),o=R(""),s=R("");function l(m){return typeof m!="string"||m.length===0?_("common.unknown"):m.replaceAll("_"," ").split(" ").map($=>$.charAt(0).toUpperCase()+$.slice(1)).join(" ")}function d(m){const $=`channels.names.${m}`,P=_($);return P===$?l(m):P}const c=te(()=>{var m,$,P,U,Se;return[{label:_("overview.version"),value:((m=a(r))==null?void 0:m.version)??_("common.na")},{label:_("overview.uptime"),value:typeof(($=a(r))==null?void 0:$.uptime_seconds)=="number"?qd(a(r).uptime_seconds):_("common.na")},{label:_("overview.model"),value:((P=a(r))==null?void 0:P.model)??_("common.na")},{label:_("overview.memoryBackend"),value:((U=a(r))==null?void 0:U.memory_backend)??_("common.na")},{label:_("overview.gatewayPort"),value:(Se=a(r))!=null&&Se.gateway_port?String(a(r).gateway_port):_("common.na")}]}),f=te(()=>{var m;return Array.isArray((m=a(r))==null?void 0:m.channels)?a(r).channels:[]});async function h(){try{const m=await St.getStatus();p(r,m,!0),p(o,""),p(s,new Date().toLocaleTimeString(),!0)}catch(m){p(o,m instanceof Error?m.message:_("overview.loadFailed"),!0)}finally{p(n,!1)}}Ft(()=>{let m=!1;const $=async()=>{m||await h()};$();const P=setInterval($,3e4);return()=>{m=!0,clearInterval(P)}});var k=tc(),w=i(k),L=i(w),T=i(L),j=g(L,2);{var N=m=>{var $=Gd(),P=i($);M(U=>b(P,U),[()=>_("common.updatedAt",{time:a(s)})]),v(m,$)};z(j,m=>{a(s)&&m(N)})}var I=g(w,2);{var X=m=>{var $=Kd(),P=i($);M(U=>b(P,U),[()=>_("overview.loading")]),v(m,$)},q=m=>{var $=Jd(),P=i($);M(()=>b(P,a(o))),v(m,$)},A=m=>{var $=ec(),P=Ee($);Qe(P,21,()=>a(c),rt,(K,J)=>{var ee=Yd(),Ie=i(ee),D=i(Ie),W=g(Ie,2),be=i(W);M(()=>{b(D,a(J).label),b(be,a(J).value)}),v(K,ee)});var U=g(P,2),Se=i(U),me=i(Se),Fe=g(Se,2);{var Ge=K=>{var J=Xd(),ee=i(J);M(Ie=>b(ee,Ie),[()=>_("overview.noChannelsConfigured")]),v(K,J)},B=K=>{var J=Zd();Qe(J,21,()=>a(f),rt,(ee,Ie)=>{var D=Qd(),W=i(D);M(be=>b(W,be),[()=>d(a(Ie))]),v(ee,D)}),v(K,J)};z(Fe,K=>{a(f).length===0?K(Ge):K(B,-1)})}M(K=>b(me,K),[()=>_("overview.configuredChannels")]),v(m,$)};z(I,m=>{a(n)?m(X):a(o)?m(q,1):m(A,-1)})}M(m=>b(T,m),[()=>_("overview.title")]),v(e,k),Oe()}var ac=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),nc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),oc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),sc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),ic=x('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),lc=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div>'),dc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function cc(e,t){Pe(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R("");function l(m){return typeof m!="string"||m.length===0?_("common.unknown"):m.replaceAll("_"," ").split(" ").map($=>$.charAt(0).toUpperCase()+$.slice(1)).join(" ")}function d(m){const $=`channels.names.${m}`,P=_($);return P===$?l(m):P}async function c(){try{const m=await St.getSessions();p(r,Array.isArray(m)?m:[],!0),p(o,""),p(s,new Date().toLocaleTimeString(),!0)}catch(m){p(o,m instanceof Error?m.message:_("sessions.loadFailed"),!0)}finally{p(n,!1)}}function f(m){$r(`/chat/${encodeURIComponent(m)}`)}Ft(()=>{let m=!1;const $=async()=>{m||await c()};$();const P=setInterval($,15e3);return()=>{m=!0,clearInterval(P)}});var h=dc(),k=i(h),w=i(k),L=i(w),T=g(w,2);{var j=m=>{var $=ac(),P=i($);M(U=>b(P,U),[()=>_("common.updatedAt",{time:a(s)})]),v(m,$)};z(T,m=>{a(s)&&m(j)})}var N=g(k,2);{var I=m=>{var $=nc(),P=i($);M(U=>b(P,U),[()=>_("sessions.loading")]),v(m,$)},X=m=>{var $=oc(),P=i($);M(()=>b(P,a(o))),v(m,$)},q=m=>{var $=sc(),P=i($);M(U=>b(P,U),[()=>_("sessions.none")]),v(m,$)},A=m=>{var $=lc(),P=i($),U=i(P),Se=i(U),me=i(Se),Fe=i(me),Ge=g(me),B=i(Ge),K=g(Ge),J=i(K),ee=g(K),Ie=i(ee),D=g(ee),W=i(D),be=g(U);Qe(be,21,()=>a(r),rt,(Ne,ie)=>{var Q=ic(),ye=i(Q),Ve=i(ye),ge=g(ye),tt=i(ge),at=g(ge),gt=i(at),H=g(at),Y=i(H),he=g(H),nt=i(he);M((Ze,ot)=>{b(Ve,a(ie).session_id),b(tt,a(ie).sender),b(gt,Ze),b(Y,a(ie).message_count),b(nt,ot)},[()=>d(a(ie).channel),()=>a(ie).last_message_preview||_("common.empty")]),G("click",Q,()=>f(a(ie).session_id)),v(Ne,Q)}),M((Ne,ie,Q,ye,Ve)=>{b(Fe,Ne),b(B,ie),b(J,Q),b(Ie,ye),b(W,Ve)},[()=>_("sessions.sessionId"),()=>_("sessions.sender"),()=>_("sessions.channel"),()=>_("sessions.messages"),()=>_("sessions.lastMessage")]),v(m,$)};z(N,m=>{a(n)?m(I):a(o)?m(X,1):a(r).length===0?m(q,2):m(A,-1)})}M(m=>b(L,m),[()=>_("sessions.title")]),v(e,h),Oe()}ir(["click"]);var uc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),fc=x('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),vc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),gc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),pc=x('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),bc=x('<img alt="Attachment" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),yc=x('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),hc=x("<div></div>"),mc=x('<div class="space-y-3"></div>'),_c=x('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),xc=x('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),kc=x('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),wc=x('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white">Remove</button></div>'),Sc=x('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),Ac=x('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region" aria-label="Chat messages"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" title="Attach files" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function Ec(e,t){Pe(t,!0);const r=10,n=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let o=aa(t,"sessionId",3,""),s=R(mt([])),l=R(""),d=R(!0),c=R(!1),f=R(""),h=R(null),k=R(null),w=R(mt([])),L=R(!1),T=0;function j(){$r("/sessions")}function N(S){return S==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":S==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function I(S){return((S==null?void 0:S.type)||"").startsWith("image/")}function X(S){return((S==null?void 0:S.type)||"").startsWith("video/")}function q(S){if(!Number.isFinite(S)||S<=0)return"0 B";const V=["B","KB","MB","GB"];let u=S,y=0;for(;u>=1024&&y<V.length-1;)u/=1024,y+=1;return`${u.toFixed(y===0?0:1)} ${V[y]}`}function A(S){return typeof S=="string"&&S.trim().length>0?S:"unknown"}function m(S){const V=I(S),u=X(S);return{id:`${S.name}-${S.lastModified}-${Math.random().toString(36).slice(2)}`,file:S,name:S.name,size:S.size,type:A(S.type),isImage:V,isVideo:u,previewUrl:V||u?URL.createObjectURL(S):""}}function $(S){S&&typeof S.previewUrl=="string"&&S.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(S.previewUrl)}function P(){for(const S of a(w))$(S);p(w,[],!0),a(k)&&(a(k).value="")}function U(S){if(!S||S.length===0||a(c))return;const V=Array.from(S),u=[],y=Math.max(0,r-a(w).length);for(const C of V.slice(0,y))u.push(m(C));p(w,[...a(w),...u],!0)}function Se(S){const V=a(w).find(u=>u.id===S);V&&$(V),p(w,a(w).filter(u=>u.id!==S),!0)}function me(){var S;a(c)||(S=a(k))==null||S.click()}function Fe(S){var V;U((V=S.currentTarget)==null?void 0:V.files),a(k)&&(a(k).value="")}function Ge(S){S.preventDefault(),!a(c)&&(T+=1,p(L,!0))}function B(S){S.preventDefault(),!a(c)&&S.dataTransfer&&(S.dataTransfer.dropEffect="copy")}function K(S){S.preventDefault(),T=Math.max(0,T-1),T===0&&p(L,!1)}function J(S){var V;S.preventDefault(),T=0,p(L,!1),U((V=S.dataTransfer)==null?void 0:V.files)}function ee(S){const V=(S||"").trim();if(!V)return"";const u=V.toLowerCase();return u.startsWith("data:image/")||u.startsWith("data:video/")||u.startsWith("http://")||u.startsWith("https://")?V:St.getSessionMediaUrl(V)}function Ie(S,V){const u=(V||"").trim().toLowerCase();return S==="VIDEO"||u.startsWith("data:video/")?"video":u.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(C=>u.endsWith(C))?"video":"image"}function D(S){if(typeof S!="string"||S.length===0)return[];const V=[];n.lastIndex=0;let u=0,y;for(;(y=n.exec(S))!==null;){y.index>u&&V.push({id:`text-${u}`,kind:"text",value:S.slice(u,y.index)});const C=(y[1]||"").toUpperCase(),O=(y[2]||y[3]||"").trim();if(O){const F=Ie(C,O);V.push({id:`${F}-${y.index}`,kind:F,value:O})}u=n.lastIndex}return u<S.length&&V.push({id:`text-tail-${u}`,kind:"text",value:S.slice(u)}),V}async function W(){await ps(),a(h)&&(a(h).scrollTop=a(h).scrollHeight)}async function be(){try{const S=await St.getSessionMessages(o());p(s,Array.isArray(S)?S:[],!0),p(f,""),await W()}catch(S){p(f,S instanceof Error?S.message:_("chat.loadFailed"),!0)}finally{p(d,!1)}}async function Ne(){const S=a(l).trim(),V=a(w).map(y=>y.file);if(S.length===0&&V.length===0||a(c))return;p(c,!0),p(l,""),p(f,"");const u=V.length>0;u||(p(s,[...a(s),{role:"user",content:S}],!0),await W());try{const y=u?await St.sendMessageWithMedia(o(),S,V):await St.sendMessage(o(),S);u?await be():y&&typeof y.reply=="string"&&y.reply.length>0&&p(s,[...a(s),{role:"assistant",content:y.reply}],!0),P()}catch(y){p(f,y instanceof Error?y.message:_("chat.sendFailed"),!0),await be()}finally{p(c,!1),await W()}}function ie(S){S.preventDefault(),Ne()}Ft(()=>{let S=!1;return(async()=>{S||(p(d,!0),await be())})(),()=>{S=!0}}),Nl(()=>{for(const S of a(w))$(S)});var Q=Ac(),ye=i(Q),Ve=i(ye),ge=i(Ve),tt=i(ge),at=g(ge,2),gt=i(at),H=g(Ve,2),Y=i(H),he=g(ye,2);{var nt=S=>{var V=uc(),u=i(V);M(()=>b(u,a(f))),v(S,V)};z(he,S=>{a(f)&&S(nt)})}var Ze=g(he,2),ot=i(Ze),dt=i(ot);{var Re=S=>{var V=fc(),u=i(V);M(()=>b(u,`Drop files to attach (${a(w).length??""}/10 selected)`)),v(S,V)};z(dt,S=>{a(L)&&S(Re)})}var ze=g(dt,2);{var st=S=>{var V=vc(),u=i(V);M(y=>b(u,y),[()=>_("chat.loading")]),v(S,V)},Ue=S=>{var V=gc(),u=i(V);M(y=>b(u,y),[()=>_("chat.empty")]),v(S,V)},_t=S=>{var V=mc();Qe(V,21,()=>a(s),rt,(u,y)=>{var C=hc();Qe(C,21,()=>D(a(y).content),O=>O.id,(O,F)=>{var Z=He(),_e=Ee(Z);{var le=ne=>{var oe=He(),$e=Ee(oe);{var se=de=>{var ce=pc(),pe=i(ce);M(()=>b(pe,a(F).value)),v(de,ce)},ae=te(()=>a(F).value.trim().length>0);z($e,de=>{a(ae)&&de(se)})}v(ne,oe)},ke=ne=>{var oe=bc();M($e=>ut(oe,"src",$e),[()=>ee(a(F).value)]),v(ne,oe)},Ke=ne=>{var oe=yc();M($e=>ut(oe,"src",$e),[()=>ee(a(F).value)]),v(ne,oe)};z(_e,ne=>{a(F).kind==="text"?ne(le):a(F).kind==="image"?ne(ke,1):a(F).kind==="video"&&ne(Ke,2)})}v(O,Z)}),M(O=>Ye(C,1,O),[()=>ks(N(a(y).role))]),v(u,C)}),v(S,V)};z(ze,S=>{a(d)?S(st):a(s).length===0?S(Ue,1):S(_t,-1)})}jn(ot,S=>p(h,S),()=>a(h));var ue=g(ot,2),je=i(ue);jn(je,S=>p(k,S),()=>a(k));var Ae=g(je,2);{var Te=S=>{var V=Sc(),u=i(V),y=i(u),C=g(u,2);Qe(C,21,()=>a(w),O=>O.id,(O,F)=>{var Z=wc(),_e=i(Z);{var le=ce=>{var pe=_c();M(()=>{ut(pe,"src",a(F).previewUrl),ut(pe,"alt",a(F).name)}),v(ce,pe)},ke=ce=>{var pe=xc();pe.muted=!0,M(()=>ut(pe,"src",a(F).previewUrl)),v(ce,pe)},Ke=ce=>{var pe=kc();v(ce,pe)};z(_e,ce=>{a(F).isImage?ce(le):a(F).isVideo?ce(ke,1):ce(Ke,-1)})}var ne=g(_e,2),oe=i(ne),$e=i(oe),se=g(oe,2),ae=i(se),de=g(ne,2);M(ce=>{b($e,a(F).name),b(ae,`${a(F).type??""} · ${ce??""}`)},[()=>q(a(F).size)]),G("click",de,()=>Se(a(F).id)),v(O,Z)}),M(()=>b(y,`Attachments (${a(w).length??""}/10)`)),v(S,V)};z(Ae,S=>{a(w).length>0&&S(Te)})}var it=g(Ae,2),re=i(it),yt=g(re,2),lt=i(yt);Rd(lt,{size:16});var xt=g(yt,2),kt=i(xt);M((S,V,u,y,C,O)=>{b(tt,S),b(gt,`${V??""}: ${o()??""}`),b(Y,u),Ye(ot,1,`flex-1 overflow-y-auto p-4 ${a(L)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),ut(re,"placeholder",y),yt.disabled=a(c)||a(w).length>=r,xt.disabled=C,b(kt,O)},[()=>_("chat.title"),()=>_("chat.session"),()=>_("chat.back"),()=>_("chat.inputPlaceholder"),()=>a(c)||!a(l).trim()&&a(w).length===0,()=>a(c)?_("chat.sending"):_("chat.send")]),G("click",H,j),xr("dragenter",Ze,Ge),xr("dragover",Ze,B),xr("dragleave",Ze,K),xr("drop",Ze,J),xr("submit",ue,ie),G("change",je,Fe),Br(re,()=>a(l),S=>p(l,S)),G("click",yt,me),v(e,Q),Oe()}ir(["click","change"]);var $c=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Cc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Mc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Nc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Tc=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),Pc=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Oc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Ic(e,t){Pe(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R("");function l(A){return typeof A!="string"||A.length===0?_("common.unknown"):A.replaceAll("_"," ").split(" ").map(m=>m.charAt(0).toUpperCase()+m.slice(1)).join(" ")}function d(A){const m=`channels.names.${A}`,$=_(m);return $===m?l(A):$}async function c(){try{const A=await St.getChannelsStatus();p(r,Array.isArray(A==null?void 0:A.channels)?A.channels:[],!0),p(o,""),p(s,new Date().toLocaleTimeString(),!0)}catch(A){p(o,A instanceof Error?A.message:_("channels.loadFailed"),!0)}finally{p(n,!1)}}Ft(()=>{let A=!1;const m=async()=>{A||await c()};m();const $=setInterval(m,3e4);return()=>{A=!0,clearInterval($)}});var f=Oc(),h=i(f),k=i(h),w=i(k),L=g(k,2);{var T=A=>{var m=$c(),$=i(m);M(P=>b($,P),[()=>_("common.updatedAt",{time:a(s)})]),v(A,m)};z(L,A=>{a(s)&&A(T)})}var j=g(h,2);{var N=A=>{var m=Cc(),$=i(m);M(P=>b($,P),[()=>_("channels.loading")]),v(A,m)},I=A=>{var m=Mc(),$=i(m);M(()=>b($,a(o))),v(A,m)},X=A=>{var m=Nc(),$=i(m);M(P=>b($,P),[()=>_("channels.noChannels")]),v(A,m)},q=A=>{var m=Pc();Qe(m,21,()=>a(r),rt,($,P)=>{var U=Tc(),Se=i(U),me=i(Se),Fe=i(me),Ge=g(me,2),B=i(Ge),K=g(Se,2),J=i(K);M((ee,Ie,D,W)=>{b(Fe,ee),Ye(Ge,1,`rounded-full px-2 py-1 text-xs font-medium ${a(P).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),b(B,Ie),b(J,`${D??""}: ${W??""}`)},[()=>d(a(P).name),()=>a(P).enabled?_("common.enabled"):_("common.disabled"),()=>_("channels.type"),()=>d(a(P).type)]),v($,U)}),v(A,m)};z(j,A=>{a(n)?A(N):a(o)?A(I,1):a(r).length===0?A(X,2):A(q,-1)})}M(A=>b(w,A),[()=>_("channels.title")]),v(e,f),Oe()}function kn(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;")}const So=/(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;function Lc(e){return e.startsWith('"')?e.endsWith(":")?"text-sky-300":"text-emerald-300":e==="true"||e==="false"?"text-amber-300":e==="null"?"text-fuchsia-300":"text-violet-300"}function Fc(e){if(!e)return"";let t="",r=0;So.lastIndex=0;for(const n of e.matchAll(So)){const o=n.index??0,s=n[0];t+=kn(e.slice(r,o)),t+=`<span class="${Lc(s)}">${kn(s)}</span>`,r=o+s.length}return t+=kn(e.slice(r)),t}var Rc=x('<span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>'),jc=x('<button type="button"><span></span></button>'),Hc=x("<option> </option>"),Dc=x('<select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select>'),zc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Uc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400">×</button></div>'),Bc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400">+ 添加</button></div>'),Wc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"> </button></div>'),Vc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),qc=x('<div><div class="flex items-start justify-between gap-3"><div class="flex-1 min-w-0"><label class="block text-sm font-medium text-gray-700 dark:text-gray-200"> <!></label> <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex-shrink-0 w-64"><!></div></div></div>'),Gc=x('<button type="button"><span></span></button>'),Kc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Jc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800">×</button></div>'),Yc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600">+ 添加</button></div>'),Xc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400"> </button></div>'),Qc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/>'),Zc=x('<textarea class="w-full rounded-lg border border-gray-300 bg-white font-mono text-xs leading-relaxed p-2 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 resize-y"></textarea>'),eu=x('<span class="text-xs text-sky-500">已修改</span>'),tu=x('<div class="mb-2 flex items-center gap-2"><!> <span class="font-mono text-xs font-medium text-gray-600 dark:text-gray-300"> </span> <!></div> <!>',1),ru=x('<span class="ml-1.5 text-xs text-sky-500">已修改</span>'),au=x('<div class="flex items-center justify-between gap-3"><span class="min-w-0 flex-1 font-mono text-sm text-gray-700 dark:text-gray-200"> <!></span> <div class="w-56 flex-shrink-0"><!></div></div>'),nu=x("<div><!></div>"),ou=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),su=x('<span class="ml-auto text-xs text-gray-400"> </span>'),iu=x('<details class="rounded-lg border border-gray-200 dark:border-gray-700"><summary class="cursor-pointer select-none flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 dark:text-gray-200 hover:bg-gray-50 dark:hover:bg-gray-700/50 rounded-lg"><span class="font-mono"> </span> <!> <!></summary> <div class="border-t border-gray-200 px-3 py-2 space-y-2 dark:border-gray-700"><!></div></details>'),lu=x('<div class="space-y-2"></div>'),du=x('<p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>'),cu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),uu=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950"><pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code><!></code></pre></div>'),fu=x('<span class="rounded-full bg-gray-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-gray-500 dark:bg-gray-700 dark:text-gray-300">Auto</span>'),vu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),gu=x('<button type="button"><span> </span> <!> <!></button>'),pu=x('<span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),bu=x('<div class="mt-2 border-t border-gray-100 pt-3 dark:border-gray-700/60"><p class="mb-2 text-xs font-medium uppercase tracking-wider text-gray-400 dark:text-gray-500">其他子配置</p> <div class="space-y-2"></div></div>'),yu=x('<details class="group scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100"><!> <span> </span> <!></summary> <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700"><!> <!></div></details>'),hu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),mu=x('<details class="scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 flex items-center gap-2 dark:text-gray-100"><!> <span class="font-mono text-sm font-semibold text-gray-800 dark:text-gray-100"> </span> <!> <span class="ml-auto text-xs text-gray-400 dark:text-gray-500"> </span></summary> <div class="border-t border-gray-200 px-4 py-3 dark:border-gray-700"><!></div></details>'),_u=x('<div class="pt-1"><p class="mb-2 px-1 text-xs font-semibold uppercase tracking-wider text-gray-400 dark:text-gray-500">自动发现的配置项</p> <div class="space-y-3"></div></div>'),xu=x('<div class="space-y-3"><div class="sticky top-0 z-20 -mx-1 overflow-x-auto rounded-xl border border-gray-200 bg-white/95 px-3 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex min-w-max items-center gap-2"></div></div> <!> <!></div>'),ku=x('<div class="flex items-start gap-2 text-xs flex-wrap"><span class="flex-shrink-0 text-gray-400 dark:text-gray-500"> </span> <span class="font-medium text-gray-600 dark:text-gray-300"> </span> <span class="text-red-500 line-through dark:text-red-400 break-all"> </span> <span class="text-gray-400 dark:text-gray-600">→</span> <span class="text-green-600 dark:text-green-400 break-all"> </span></div>'),wu=x('<div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950"><p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p> <div class="space-y-1.5 max-h-48 overflow-y-auto"></div></div>'),Su=x('<div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95"><div class="mx-auto flex max-w-5xl items-center justify-between gap-4"><div class="flex items-center gap-3"><span class="text-sm text-sky-600 dark:text-sky-400"> </span> <button type="button" class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"> </button></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">放弃修改</button> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div> <!></div>'),Au=x("<div> </div>"),Eu=x('<section class="space-y-4 pb-24"><div class="flex items-center justify-between gap-4"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">复制 JSON</button></div></div> <!> <!> <!></section>');function $u(e,t){Pe(t,!0);const r=(u,y=Ce,C=Ce)=>{const O=te(()=>B(y())),F=te(()=>a(be).has(y())),Z=te(()=>a(I).has(y()));var _e=qc(),le=i(_e),ke=i(le),Ke=i(ke),ne=i(Ke),oe=g(ne);{var $e=Be=>{var ve=Rc();v(Be,ve)};z(oe,Be=>{a(F)&&Be($e)})}var se=g(Ke,2),ae=i(se),de=g(ke,2),ce=i(de);{var pe=Be=>{var ve=jc(),Je=i(ve);M(()=>{Ye(ve,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${a(O)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(Je,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${a(O)?"translate-x-6":"translate-x-1"}`)}),G("click",ve,()=>ge(y(),!a(O))),v(Be,ve)},we=Be=>{var ve=Dc();Qe(ve,21,()=>C().options,rt,(ct,Lt)=>{var qe=Hc(),At=i(qe),Ht={};M(()=>{b(At,a(Lt)||"(默认)"),Ht!==(Ht=a(Lt))&&(qe.value=(qe.__value=a(Lt))??"")}),v(ct,qe)});var Je;eo(ve),M(()=>{Je!==(Je=a(O)??C().default)&&(ve.value=(ve.__value=a(O)??C().default)??"",Fa(ve,a(O)??C().default))}),G("change",ve,ct=>ge(y(),ct.target.value)),v(Be,ve)},fe=Be=>{var ve=zc();M(Je=>{hr(ve,a(O)??C().default),ut(ve,"min",C().min),ut(ve,"max",C().max),ut(ve,"step",C().step??1),ut(ve,"placeholder",Je)},[()=>String(C().default)]),G("input",ve,Je=>{const ct=C().step&&C().step<1?parseFloat(Je.target.value):parseInt(Je.target.value,10);isNaN(ct)||ge(y(),ct)}),v(Be,ve)},xe=Be=>{var ve=Bc(),Je=i(ve);{var ct=At=>{var Ht=He(),Ar=Ee(Ht);Qe(Ar,17,()=>a(O),rt,(Rt,Ba,Aa)=>{var ea=Uc(),ta=i(ea),fn=g(ta,2);M(()=>hr(ta,a(Ba))),G("input",ta,vn=>gt(y(),Aa,vn.target.value)),G("click",fn,()=>at(y(),Aa)),v(Rt,ea)}),v(At,Ht)},Lt=te(()=>Array.isArray(a(O)));z(Je,At=>{a(Lt)&&At(ct)})}var qe=g(Je,2);G("click",qe,()=>tt(y())),v(Be,ve)},et=Be=>{var ve=Wc(),Je=i(ve),ct=g(Je,2),Lt=i(ct);M(()=>{ut(Je,"type",a(Z)?"text":"password"),hr(Je,a(O)??""),ut(Je,"placeholder",C().default||"未设置"),b(Lt,a(Z)?"隐藏":"显示")}),G("input",Je,qe=>ge(y(),qe.target.value)),G("click",ct,()=>H(y())),v(Be,ve)},ht=Be=>{var ve=Vc();M(()=>{hr(ve,a(O)??""),ut(ve,"placeholder",C().default||"未设置")}),G("input",ve,Je=>ge(y(),Je.target.value)),v(Be,ve)};z(ce,Be=>{C().type==="bool"?Be(pe):C().type==="enum"?Be(we,1):C().type==="number"?Be(fe,2):C().type==="array"?Be(xe,3):C().sensitive?Be(et,4):Be(ht,-1)})}M(()=>{Ye(_e,1,`rounded-lg border p-3 transition-colors ${a(F)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`),b(ne,`${C().label??""} `),b(ae,C().desc)}),v(u,_e)},n=(u,y=Ce,C=Ce)=>{const O=te(()=>$(y().split(".").pop()??"")),F=te(()=>a(I).has(y()));var Z=He(),_e=Ee(Z);{var le=se=>{var ae=Gc(),de=i(ae);M(()=>{Ye(ae,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${C()?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(de,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${C()?"translate-x-6":"translate-x-1"}`)}),G("click",ae,()=>ge(y(),!C())),v(se,ae)},ke=se=>{var ae=Kc();M(()=>hr(ae,C())),G("input",ae,de=>{const ce=parseFloat(de.target.value);isNaN(ce)||ge(y(),ce)}),v(se,ae)},Ke=se=>{var ae=Yc(),de=i(ae);Qe(de,17,C,rt,(pe,we,fe)=>{var xe=Jc(),et=i(xe),ht=g(et,2);M(()=>hr(et,a(we))),G("input",et,Be=>{const ve=[...Fe(a(c),y())||[]];ve[fe]=Be.target.value,ge(y(),ve)}),G("click",ht,()=>{const Be=(Fe(a(c),y())||[]).filter((ve,Je)=>Je!==fe);ge(y(),Be)}),v(pe,xe)});var ce=g(de,2);G("click",ce,()=>{const pe=[...Fe(a(c),y())||[],""];ge(y(),pe)}),v(se,ae)},ne=te(()=>Array.isArray(C())),oe=se=>{var ae=Xc(),de=i(ae),ce=g(de,2),pe=i(ce);M(()=>{ut(de,"type",a(F)?"text":"password"),hr(de,C()??""),b(pe,a(F)?"隐藏":"显示")}),G("input",de,we=>ge(y(),we.target.value)),G("click",ce,()=>H(y())),v(se,ae)},$e=se=>{var ae=Qc();M(()=>hr(ae,C()??"")),G("input",ae,de=>ge(y(),de.target.value)),v(se,ae)};z(_e,se=>{typeof C()=="boolean"?se(le):typeof C()=="number"?se(ke,1):a(ne)?se(Ke,2):a(O)?se(oe,3):se($e,-1)})}v(u,Z)},o=(u,y=Ce,C=Ce)=>{const O=te(()=>JSON.stringify(C(),null,2)),F=te(()=>Math.min(15,(a(O).match(/\n/g)||[]).length+2));var Z=Zc();M(()=>{hr(Z,a(O)),ut(Z,"rows",a(F))}),xr("blur",Z,_e=>{try{const le=JSON.parse(_e.target.value);ge(y(),le)}catch{_e.target.value=JSON.stringify(Fe(a(c),y())??C(),null,2)}}),v(u,Z)},s=(u,y=Ce,C=Ce,O=Ce)=>{const F=te(()=>Fe(a(c),y())??O()),Z=te(()=>a(be).has(y()));var _e=nu(),le=i(_e);{var ke=oe=>{var $e=tu(),se=Ee($e),ae=i(se);Cd(ae,{size:13,class:"flex-shrink-0 text-gray-400"});var de=g(ae,2),ce=i(de),pe=g(de,2);{var we=xe=>{var et=eu();v(xe,et)};z(pe,xe=>{a(Z)&&xe(we)})}var fe=g(se,2);o(fe,y,()=>a(F)),M(()=>b(ce,C())),v(oe,$e)},Ke=te(()=>he(a(F))),ne=oe=>{var $e=au(),se=i($e),ae=i(se),de=g(ae);{var ce=fe=>{var xe=ru();v(fe,xe)};z(de,fe=>{a(Z)&&fe(ce)})}var pe=g(se,2),we=i(pe);n(we,y,()=>a(F)),M(()=>b(ae,`${C()??""} `)),v(oe,$e)};z(le,oe=>{a(Ke)?oe(ke):oe(ne,-1)})}M(()=>Ye(_e,1,`rounded-lg border p-3 transition-colors ${a(Z)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`)),v(u,_e)},l=(u,y=Ce,C=Ce,O=Ce)=>{const F=te(()=>A(O())),Z=te(()=>Ne(y()));var _e=iu(),le=i(_e),ke=i(le),Ke=i(ke),ne=g(ke,2);{var oe=we=>{var fe=ou();v(we,fe)};z(ne,we=>{a(Z)&&we(oe)})}var $e=g(ne,2);{var se=we=>{var fe=su(),xe=i(fe);M(et=>b(xe,et),[()=>m(O())]),v(we,fe)};z($e,we=>{a(F)||we(se)})}var ae=g(le,2),de=i(ae);{var ce=we=>{var fe=He(),xe=Ee(fe);Qe(xe,17,()=>Object.entries(O()),rt,(et,ht)=>{var Be=te(()=>Na(a(ht),2));let ve=()=>a(Be)[0],Je=()=>a(Be)[1];const ct=te(()=>`${y()}.${ve()}`);var Lt=He(),qe=Ee(Lt);{var At=Rt=>{s(Rt,()=>a(ct),ve,Je)},Ht=te(()=>A(Je())),Ar=Rt=>{s(Rt,()=>a(ct),ve,Je)};z(qe,Rt=>{a(Ht)?Rt(At):Rt(Ar,-1)})}v(et,Lt)}),v(we,fe)},pe=we=>{s(we,y,C,O)};z(de,we=>{a(F)?we(ce):we(pe,-1)})}M(()=>b(Ke,C())),v(u,_e)},d=(u,y=Ce,C=Ce)=>{var O=He(),F=Ee(O);{var Z=ne=>{var oe=lu();Qe(oe,21,()=>Object.entries(C()),rt,($e,se)=>{var ae=te(()=>Na(a(se),2));let de=()=>a(ae)[0],ce=()=>a(ae)[1];var pe=He(),we=Ee(pe);{var fe=ht=>{l(ht,()=>`${y()}.${de()}`,de,ce)},xe=te(()=>A(ce())),et=ht=>{s(ht,()=>`${y()}.${de()}`,de,ce)};z(we,ht=>{a(xe)?ht(fe):ht(et,-1)})}v($e,pe)}),v(ne,oe)},_e=te(()=>A(C())),le=ne=>{s(ne,y,y,C)},ke=te(()=>Array.isArray(C())),Ke=ne=>{s(ne,y,y,C)};z(F,ne=>{a(_e)?ne(Z):a(ke)?ne(le,1):ne(Ke,-1)})}v(u,O)};let c=R(null),f=R(null),h=R(null),k=R(!0),w=R(!1),L=R(""),T=R(""),j=R(!1),N=R(!1),I=R(mt(new Set)),X=R("provider");const q={provider:Ud,gateway:Pd,channels:Ld,agent:xd,memory:kd,security:Dd,heartbeat:Od,reliability:Ps,scheduler:$d,sessions_spawn:Td,observability:Sd,web_search:jd,cost:Nd,runtime:Hd,tunnel:wd,identity:_d};function A(u){return u!==null&&typeof u=="object"&&!Array.isArray(u)}function m(u){return typeof u=="boolean"?"bool":typeof u=="number"?"number":Array.isArray(u)?"array":A(u)?"object":"string"}function $(u){const y=String(u).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(C=>y.includes(C))}function P(u){if(!a(c))return[];const y=new Set(Object.keys(u.fields)),C=new Set;for(const F of Object.keys(u.fields))C.add(F.split(".")[0]);const O=[];for(const F of C){const Z=a(c)[F];if(A(Z))for(const[_e,le]of Object.entries(Z)){const ke=`${F}.${_e}`;y.has(ke)||O.push({path:ke,key:_e,value:le})}}return O}const U=te(()=>a(c)?Object.keys(a(c)).filter(u=>!Cs.has(u)).sort():[]),Se=Object.entries(Qa),me=te(()=>Dn(a(c)));function Fe(u,y){if(!u)return;const C=y.split(".");let O=u;for(const F of C){if(O==null||typeof O!="object")return;O=O[F]}return O}function Ge(u,y,C){const O=y.split(".");let F=u;for(let Z=0;Z<O.length-1;Z++)(F[O[Z]]==null||typeof F[O[Z]]!="object")&&(F[O[Z]]={}),F=F[O[Z]];F[O[O.length-1]]=C}function B(u){if(a(c))return Fe(a(c),u)}function K(u){return JSON.parse(JSON.stringify(u))}function J(u,y){return JSON.stringify(u)===JSON.stringify(y)}function ee(u,y,C){const O=[],F=new Set([...Object.keys(u||{}),...Object.keys(y||{})]);for(const Z of F){const _e=C?`${C}.${Z}`:Z,le=(u||{})[Z],ke=(y||{})[Z];A(le)&&A(ke)?O.push(...ee(le,ke,_e)):J(le,ke)||O.push({fieldPath:_e,newVal:le,oldVal:ke})}return O}function Ie(){return!a(c)||!a(f)?[]:ee(a(c),a(f),"").map(y=>{for(const O of Object.values(Qa))if(O.fields[y.fieldPath])return{...y,label:O.fields[y.fieldPath].label,group:O.label};const C=y.fieldPath.split(".");return{...y,label:Hn(C[C.length-1]),group:Hn(C[0])}})}const D=te(()=>!!(a(c)&&a(f)&&JSON.stringify(a(c))!==JSON.stringify(a(f)))),W=te(Ie),be=te(()=>new Set(a(W).map(u=>u.fieldPath)));function Ne(u){for(const y of a(be))if(y===u||y.startsWith(u+"."))return!0;return!1}function ie(u){p(X,u,!0),Ms(u)}function Q(){if(typeof window>"u")return;const u=window.location.hash.replace(/^#/,"");if(!u.startsWith("config-section-"))return;const y=u.replace(/^config-section-/,"");a(me).some(C=>C.groupKey===y)&&ie(y)}const ye=te(()=>a(c)?JSON.stringify(a(c),null,2):""),Ve=te(()=>Fc(a(ye)));function ge(u,y){if(!a(c))return;const C=K(a(c));Ge(C,u,y),p(c,C,!0)}function tt(u){const y=B(u),C=Array.isArray(y)?[...y,""]:[""];ge(u,C)}function at(u,y){const C=B(u);Array.isArray(C)&&ge(u,C.filter((O,F)=>F!==y))}function gt(u,y,C){const O=B(u);if(!Array.isArray(O))return;const F=[...O];F[y]=C,ge(u,F)}function H(u){const y=new Set(a(I));y.has(u)?y.delete(u):y.add(u),p(I,y,!0)}function Y(u){return u==null?"null":typeof u=="boolean"?u?"true":"false":Array.isArray(u)||typeof u=="object"?JSON.stringify(u):String(u)}function he(u){return!!(A(u)||Array.isArray(u)&&u.some(y=>A(y)||Array.isArray(y)))}async function nt(){try{const[u,y]=await Promise.all([St.getConfig(),St.getStatus().catch(()=>null)]);p(c,typeof u=="object"&&u?u:{},!0),p(f,K(a(c)),!0),p(h,y,!0),p(L,"")}catch(u){p(L,u instanceof Error?u.message:"Failed to load config",!0)}finally{p(k,!1)}}async function Ze(){if(!(!a(D)||a(w))){p(w,!0),p(T,"");try{const u={};for(const C of a(W))Ge(u,C.fieldPath,C.newVal);const y=await St.saveConfig(u);p(f,K(a(c)),!0),p(N,!1),y!=null&&y.restart_required?p(T,"已保存，部分设置需要重启服务后生效"):p(T,"已保存"),setTimeout(()=>{p(T,"")},5e3)}catch(u){p(T,"保存失败: "+(u instanceof Error?u.message:String(u)))}finally{p(w,!1)}}}function ot(){a(f)&&(p(c,K(a(f)),!0),p(N,!1))}async function dt(){if(!(!a(ye)||typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(a(ye))}catch{}}Ft(()=>{nt()}),Ft(()=>{a(k)||a(j)||a(me).length===0||queueMicrotask(()=>{Q()})});var Re=Eu(),ze=i(Re),st=i(ze),Ue=i(st),_t=g(st,2),ue=i(_t),je=i(ue),Ae=g(ue,2),Te=g(ze,2);{var it=u=>{var y=du();v(u,y)},re=u=>{var y=cu(),C=i(y);M(()=>b(C,a(L))),v(u,y)},yt=u=>{var y=uu(),C=i(y),O=i(C),F=i(O);vl(F,()=>a(Ve)),v(u,y)},lt=u=>{var y=xu(),C=i(y),O=i(C);Qe(O,21,()=>a(me),rt,(le,ke)=>{const Ke=te(()=>Ne(a(ke).groupKey));var ne=gu(),oe=i(ne),$e=i(oe),se=g(oe,2);{var ae=pe=>{var we=fu();v(pe,we)};z(se,pe=>{a(ke).dynamic&&pe(ae)})}var de=g(se,2);{var ce=pe=>{var we=vu();v(pe,we)};z(de,pe=>{a(Ke)&&pe(ce)})}M(()=>{Ye(ne,1,`inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm transition ${a(X)===a(ke).groupKey?"border-sky-500 bg-sky-500/10 text-sky-700 dark:text-sky-300":"border-gray-300 bg-white text-gray-600 hover:border-sky-400 hover:text-sky-600 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:border-sky-500 dark:hover:text-sky-300"}`),b($e,a(ke).label)}),G("click",ne,()=>ie(a(ke).groupKey)),v(le,ne)});var F=g(C,2);Qe(F,17,()=>Se,rt,(le,ke)=>{var Ke=te(()=>Na(a(ke),2));let ne=()=>a(Ke)[0],oe=()=>a(Ke)[1];const $e=te(()=>q[ne()]),se=te(()=>P(oe())),ae=te(()=>Object.keys(oe().fields)),de=te(()=>a(ae).some(qe=>a(be).has(qe))||a(se).some(qe=>Ne(qe.path)));var ce=yu(),pe=i(ce),we=i(pe);{var fe=qe=>{var At=He(),Ht=Ee(At);gl(Ht,()=>a($e),(Ar,Rt)=>{Rt(Ar,{size:18,class:"text-gray-500 dark:text-gray-400"})}),v(qe,At)};z(we,qe=>{a($e)&&qe(fe)})}var xe=g(we,2),et=i(xe),ht=g(xe,2);{var Be=qe=>{var At=pu();v(qe,At)};z(ht,qe=>{a(de)&&qe(Be)})}var ve=g(pe,2),Je=i(ve);Qe(Je,17,()=>Object.entries(oe().fields),rt,(qe,At)=>{var Ht=te(()=>Na(a(At),2));r(qe,()=>a(Ht)[0],()=>a(Ht)[1])});var ct=g(Je,2);{var Lt=qe=>{var At=bu(),Ht=g(i(At),2);Qe(Ht,21,()=>a(se),rt,(Ar,Rt)=>{let Ba=()=>a(Rt).path,Aa=()=>a(Rt).key,ea=()=>a(Rt).value;var ta=He(),fn=Ee(ta);{var vn=ra=>{l(ra,Ba,Aa,ea)},Os=te(()=>A(ea())),Is=ra=>{s(ra,Ba,Aa,ea)};z(fn,ra=>{a(Os)?ra(vn):ra(Is,-1)})}v(Ar,ta)}),v(qe,At)};z(ct,qe=>{a(se).length>0&&qe(Lt)})}M(qe=>{ut(ce,"id",qe),ce.open=oe().defaultOpen,b(et,oe().label)},[()=>Za(ne())]),xr("toggle",ce,qe=>{qe.currentTarget.open&&p(X,ne(),!0)}),v(le,ce)});var Z=g(F,2);{var _e=le=>{var ke=_u(),Ke=g(i(ke),2);Qe(Ke,21,()=>a(U),rt,(ne,oe)=>{const $e=te(()=>a(c)[a(oe)]),se=te(()=>Ne(a(oe))),ae=te(()=>m(a($e)));var de=mu(),ce=i(de),pe=i(ce);Md(pe,{size:18,class:"flex-shrink-0 text-gray-400 dark:text-gray-500"});var we=g(pe,2),fe=i(we),xe=g(we,2);{var et=ct=>{var Lt=hu();v(ct,Lt)};z(xe,ct=>{a(se)&&ct(et)})}var ht=g(xe,2),Be=i(ht),ve=g(ce,2),Je=i(ve);d(Je,()=>a(oe),()=>a($e)),M(ct=>{ut(de,"id",ct),b(fe,a(oe)),b(Be,a(ae))},[()=>Za(a(oe))]),xr("toggle",de,ct=>{ct.currentTarget.open&&p(X,a(oe),!0)}),v(ne,de)}),v(le,ke)};z(Z,le=>{a(U).length>0&&le(_e)})}v(u,y)};z(Te,u=>{a(k)?u(it):a(L)?u(re,1):a(j)?u(yt,2):u(lt,-1)})}var xt=g(Te,2);{var kt=u=>{var y=Su(),C=i(y),O=i(C),F=i(O),Z=i(F),_e=g(F,2),le=i(_e),ke=g(O,2),Ke=i(ke),ne=g(Ke,2),oe=i(ne),$e=g(C,2);{var se=ae=>{var de=wu(),ce=g(i(de),2);Qe(ce,21,()=>a(W),rt,(pe,we)=>{var fe=ku(),xe=i(fe),et=i(xe),ht=g(xe,2),Be=i(ht),ve=g(ht,2),Je=i(ve),ct=g(ve,4),Lt=i(ct);M((qe,At)=>{b(et,a(we).group),b(Be,a(we).label),b(Je,qe),b(Lt,At)},[()=>Y(a(we).oldVal),()=>Y(a(we).newVal)]),v(pe,fe)}),v(ae,de)};z($e,ae=>{a(N)&&ae(se)})}M(()=>{b(Z,`${a(W).length??""} 项更改`),b(le,a(N)?"隐藏详情":"查看详情"),ne.disabled=a(w),b(oe,a(w)?"保存中...":"保存配置")}),G("click",_e,()=>p(N,!a(N))),G("click",Ke,ot),G("click",ne,Ze),v(u,y)};z(xt,u=>{a(D)&&!a(k)&&!a(j)&&u(kt)})}var S=g(xt,2);{var V=u=>{var y=Au(),C=i(y);M(O=>{Ye(y,1,`fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg ${O??""}`),b(C,a(T))},[()=>a(T).startsWith("保存失败")?"border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"]),v(u,y)};z(S,u=>{a(T)&&u(V)})}M(u=>{b(Ue,u),b(je,a(j)?"结构化编辑":"JSON 视图")},[()=>_("config.title")]),G("click",ue,()=>p(j,!a(j))),G("click",Ae,dt),v(e,Re),Oe()}ir(["click","change","input"]);var Cu=x('<p class="text-gray-400 dark:text-gray-500"> </p>'),Mu=x('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Nu=x('<ol class="space-y-1"></ol>'),Tu=x('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Pu(e,t){Pe(t,!0);const r=1e3,n=500,o=1e4;let s=R(mt([])),l=R(!1),d=R("disconnected"),c=R(null),f=null,h=null,k=0,w=!0;const L=te(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),T=te(()=>a(d)==="connected"?_("logs.connected"):a(d)==="reconnecting"?_("logs.reconnecting"):_("logs.disconnected"));function j(ie){const Q=Xa?new URL(Xa,window.location.href):new URL(window.location.href);return Q.protocol=Q.protocol==="https:"?"wss:":"ws:",Q.pathname="/api/logs/stream",Q.search=`token=${encodeURIComponent(ie)}`,Q.hash="",Q.toString()}function N(ie){if(typeof ie!="string"||ie.length===0)return;const Q=ie.split(/\r?\n/).filter(Ve=>Ve.length>0);if(Q.length===0)return;const ye=[...a(s),...Q];p(s,ye.length>r?ye.slice(ye.length-r):ye,!0)}function I(){h!==null&&(clearTimeout(h),h=null)}function X(){f&&(f.onopen=null,f.onmessage=null,f.onerror=null,f.onclose=null,f.close(),f=null)}function q(){if(!w){p(d,"disconnected");return}p(d,"reconnecting");const ie=Math.min(n*2**k,o);k+=1,I(),h=setTimeout(()=>{h=null,A()},ie)}function A(){I();const ie=Ra();if(!ie){p(d,"disconnected");return}p(d,"reconnecting"),X();let Q;try{Q=new WebSocket(j(ie))}catch{q();return}f=Q,Q.onopen=()=>{k=0,p(d,"connected")},Q.onmessage=ye=>{a(l)||N(ye.data)},Q.onerror=()=>{(Q.readyState===WebSocket.OPEN||Q.readyState===WebSocket.CONNECTING)&&Q.close()},Q.onclose=()=>{f=null,q()}}function m(){p(l,!a(l))}function $(){p(s,[],!0)}Ft(()=>(w=!0,A(),()=>{w=!1,I(),X(),p(d,"disconnected")})),Ft(()=>{a(s).length,a(l),!(a(l)||!a(c))&&queueMicrotask(()=>{a(c)&&(a(c).scrollTop=a(c).scrollHeight)})});var P=Tu(),U=i(P),Se=i(U),me=i(Se),Fe=g(Se,2),Ge=i(Fe),B=i(Ge),K=g(Ge,2),J=i(K),ee=g(K,2),Ie=i(ee),D=g(U,2),W=i(D);{var be=ie=>{var Q=Cu(),ye=i(Q);M(Ve=>b(ye,Ve),[()=>_("logs.waiting")]),v(ie,Q)},Ne=ie=>{var Q=Nu();Qe(Q,21,()=>a(s),rt,(ye,Ve,ge)=>{var tt=Mu(),at=i(tt),gt=i(at),H=g(at,2),Y=i(H);M(he=>{b(gt,he),b(Y,a(Ve))},[()=>String(ge+1).padStart(4,"0")]),v(ye,tt)}),v(ie,Q)};z(W,ie=>{a(s).length===0?ie(be):ie(Ne,-1)})}jn(D,ie=>p(c,ie),()=>a(c)),M((ie,Q,ye)=>{b(me,ie),Ye(Ge,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(L)}`),b(B,a(T)),b(J,Q),b(Ie,ye)},[()=>_("logs.title"),()=>a(l)?_("logs.resume"):_("logs.pause"),()=>_("logs.clear")]),G("click",K,m),G("click",ee,$),v(e,P),Oe()}ir(["click"]);var Ou=x("<option> </option>"),Iu=x('<div class="rounded-xl border border-sky-500/30 bg-white p-4 space-y-3 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),Lu=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Fu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Ru=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),ju=x("<option> </option>"),Hu=x('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),Du=x('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button"><span></span></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 dark:text-red-300"> </button></div></div>'),zu=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),Uu=x('<div class="space-y-3"></div>'),Bu=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <!> <!></section>');function Wu(e,t){Pe(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call_end","message_received","message_sent"];let n=R(mt([])),o=R(!0),s=R(""),l=R(null),d=R(!1),c=R(mt(r[0])),f=R(""),h=R(3e4),k=R(!0);function w(){p(c,r[0],!0),p(f,""),p(h,3e4),p(k,!0)}function L(D){return D.split("_").map(W=>W.charAt(0).toUpperCase()+W.slice(1)).join(" ")}async function T(){try{const D=await St.getHooks();p(n,Array.isArray(D==null?void 0:D.hooks)?D.hooks:[],!0),p(s,"")}catch{p(n,[{id:"1",event:"message_received",command:'echo "msg received"',timeout_ms:3e4,enabled:!0},{id:"2",event:"agent_start",command:"/opt/scripts/on-start.sh",timeout_ms:1e4,enabled:!0},{id:"3",event:"tool_call_end",command:'notify-send "tool done"',timeout_ms:5e3,enabled:!1}],!0),p(s,"")}finally{p(o,!1)}}function j(D){p(l,D.id,!0),p(c,D.event,!0),p(f,D.command,!0),p(h,D.timeout_ms,!0),p(k,D.enabled,!0)}function N(){p(l,null),w()}function I(D){p(n,a(n).map(W=>W.id===D?{...W,event:a(c),command:a(f),timeout_ms:a(h),enabled:a(k)}:W),!0),p(l,null),w()}function X(){if(!a(f).trim())return;const D={id:String(Date.now()),event:a(c),command:a(f).trim(),timeout_ms:a(h),enabled:a(k)};p(n,[...a(n),D],!0),p(d,!1),w()}function q(D){p(n,a(n).filter(W=>W.id!==D),!0)}function A(D){p(n,a(n).map(W=>W.id===D?{...W,enabled:!W.enabled}:W),!0)}Ft(()=>{T()});var m=Bu(),$=i(m),P=i($),U=i(P),Se=g(P,2),me=i(Se),Fe=g($,2);{var Ge=D=>{var W=Iu(),be=i(W),Ne=i(be),ie=g(be,2),Q=i(ie),ye=i(Q),Ve=i(ye),ge=g(ye,2);Qe(ge,21,()=>r,rt,(Te,it)=>{var re=Ou(),yt=i(re),lt={};M(xt=>{b(yt,xt),lt!==(lt=a(it))&&(re.value=(re.__value=a(it))??"")},[()=>L(a(it))]),v(Te,re)});var tt=g(Q,2),at=i(tt),gt=i(at),H=g(at,2),Y=g(tt,2),he=i(Y),nt=i(he),Ze=g(he,2),ot=g(Y,2),dt=i(ot),Re=i(dt),ze=g(dt,2),st=i(ze),Ue=g(ie,2),_t=i(Ue),ue=i(_t),je=g(_t,2),Ae=i(je);M((Te,it,re,yt,lt,xt,kt,S)=>{b(Ne,Te),b(Ve,it),b(gt,re),b(nt,yt),ut(Ze,"placeholder",lt),b(Re,xt),Ye(ze,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(k)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(st,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(k)?"translate-x-4":"translate-x-1"}`),b(ue,kt),b(Ae,S)},[()=>_("hooks.newHook"),()=>_("hooks.event"),()=>_("hooks.timeout"),()=>_("hooks.command"),()=>_("hooks.commandPlaceholder"),()=>_("hooks.enabled"),()=>_("hooks.cancel"),()=>_("hooks.save")]),Rn(ge,()=>a(c),Te=>p(c,Te)),Br(H,()=>a(h),Te=>p(h,Te)),Br(Ze,()=>a(f),Te=>p(f,Te)),G("click",ze,()=>p(k,!a(k))),G("click",_t,()=>{p(d,!1),w()}),G("click",je,X),v(D,W)};z(Fe,D=>{a(d)&&D(Ge)})}var B=g(Fe,2);{var K=D=>{var W=Lu(),be=i(W);M(Ne=>b(be,Ne),[()=>_("hooks.loading")]),v(D,W)},J=D=>{var W=Fu(),be=i(W);M(()=>b(be,a(s))),v(D,W)},ee=D=>{var W=Ru(),be=i(W);M(Ne=>b(be,Ne),[()=>_("hooks.noHooks")]),v(D,W)},Ie=D=>{var W=Uu();Qe(W,21,()=>a(n),be=>be.id,(be,Ne)=>{var ie=zu(),Q=i(ie);{var ye=ge=>{var tt=Hu(),at=i(tt),gt=i(at),H=i(gt),Y=i(H),he=g(H,2);Qe(he,21,()=>r,rt,(kt,S)=>{var V=ju(),u=i(V),y={};M(C=>{b(u,C),y!==(y=a(S))&&(V.value=(V.__value=a(S))??"")},[()=>L(a(S))]),v(kt,V)});var nt=g(gt,2),Ze=i(nt),ot=i(Ze),dt=g(Ze,2),Re=g(nt,2),ze=i(Re),st=i(ze),Ue=g(ze,2),_t=g(Re,2),ue=i(_t),je=i(ue),Ae=g(ue,2),Te=i(Ae),it=g(at,2),re=i(it),yt=i(re),lt=g(re,2),xt=i(lt);M((kt,S,V,u,y,C)=>{b(Y,kt),b(ot,S),b(st,V),b(je,u),Ye(Ae,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(k)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(Te,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(k)?"translate-x-4":"translate-x-1"}`),b(yt,y),b(xt,C)},[()=>_("hooks.event"),()=>_("hooks.timeout"),()=>_("hooks.command"),()=>_("hooks.enabled"),()=>_("hooks.cancel"),()=>_("hooks.save")]),Rn(he,()=>a(c),kt=>p(c,kt)),Br(dt,()=>a(h),kt=>p(h,kt)),Br(Ue,()=>a(f),kt=>p(f,kt)),G("click",Ae,()=>p(k,!a(k))),G("click",re,N),G("click",lt,()=>I(a(Ne).id)),v(ge,tt)},Ve=ge=>{var tt=Du(),at=i(tt),gt=i(at),H=i(gt),Y=i(H),he=g(H,2),nt=i(he),Ze=g(gt,2),ot=i(Ze),dt=g(Ze,2),Re=i(dt),ze=g(at,2),st=i(ze),Ue=i(st),_t=g(st,2),ue=i(_t),je=g(_t,2),Ae=i(je);M((Te,it,re,yt,lt)=>{b(Y,Te),Ye(he,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Ne).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),b(nt,it),b(ot,a(Ne).command),b(Re,`${re??""}: ${a(Ne).timeout_ms??""}ms`),Ye(st,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(Ne).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(Ue,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(Ne).enabled?"translate-x-4":"translate-x-1"}`),b(ue,yt),b(Ae,lt)},[()=>L(a(Ne).event),()=>a(Ne).enabled?_("common.enabled"):_("common.disabled"),()=>_("hooks.timeout"),()=>_("hooks.edit"),()=>_("hooks.delete")]),G("click",st,()=>A(a(Ne).id)),G("click",_t,()=>j(a(Ne))),G("click",je,()=>q(a(Ne).id)),v(ge,tt)};z(Q,ge=>{a(l)===a(Ne).id?ge(ye):ge(Ve,-1)})}v(be,ie)}),v(D,W)};z(B,D=>{a(o)?D(K):a(s)?D(J,1):a(n).length===0?D(ee,2):D(Ie,-1)})}M((D,W)=>{b(U,D),b(me,W)},[()=>_("hooks.title"),()=>a(d)?_("hooks.cancelAdd"):_("hooks.addHook")]),G("click",Se,()=>{p(d,!a(d)),a(d)&&w()}),v(e,m),Oe()}ir(["click"]);var Vu=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),qu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Gu=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Ku=x('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),Ju=x('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),Yu=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),Xu=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Qu=x('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),Zu=x('<div class="space-y-4"></div>'),e0=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function t0(e,t){Pe(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R(null);async function l(){try{const A=await St.getMcpServers();p(r,Array.isArray(A==null?void 0:A.servers)?A.servers:[],!0),p(o,"")}catch{p(r,[{name:"filesystem",url:"stdio:///usr/local/bin/mcp-filesystem",status:"connected",tools:[{name:"read_file",description:"Read contents of a file"},{name:"write_file",description:"Write content to a file"},{name:"list_directory",description:"List directory contents"}]},{name:"github",url:"https://mcp.github.com/sse",status:"connected",tools:[{name:"search_repositories",description:"Search GitHub repositories"},{name:"create_issue",description:"Create a new issue"},{name:"list_pull_requests",description:"List pull requests"}]},{name:"database",url:"stdio:///opt/mcp/db-server",status:"disconnected",tools:[]}],!0),p(o,"")}finally{p(n,!1)}}function d(A){p(s,a(s)===A?null:A,!0)}async function c(){p(n,!0),await l()}Ft(()=>{l()});var f=e0(),h=i(f),k=i(h),w=i(k),L=g(k,2),T=i(L),j=g(h,2);{var N=A=>{var m=Vu(),$=i(m);M(P=>b($,P),[()=>_("mcp.loading")]),v(A,m)},I=A=>{var m=qu(),$=i(m);M(()=>b($,a(o))),v(A,m)},X=A=>{var m=Gu(),$=i(m);M(P=>b($,P),[()=>_("mcp.noServers")]),v(A,m)},q=A=>{var m=Zu();Qe(m,21,()=>a(r),rt,($,P)=>{var U=Qu(),Se=i(U),me=i(Se),Fe=i(me),Ge=i(Fe),B=i(Ge),K=g(Ge,2),J=i(K),ee=g(Fe,2),Ie=i(ee),D=g(me,2),W=i(D),be=g(Se,2);{var Ne=Q=>{var ye=Yu(),Ve=i(ye),ge=i(Ve),tt=g(Ve,2);Qe(tt,21,()=>a(P).tools,rt,(at,gt)=>{var H=Ju(),Y=i(H),he=i(Y),nt=g(Y,2);{var Ze=ot=>{var dt=Ku(),Re=i(dt);M(()=>b(Re,a(gt).description)),v(ot,dt)};z(nt,ot=>{a(gt).description&&ot(Ze)})}M(()=>b(he,a(gt).name)),v(at,H)}),M(at=>b(ge,at),[()=>_("mcp.availableTools")]),v(Q,ye)},ie=Q=>{var ye=Xu(),Ve=i(ye),ge=i(Ve);M(tt=>b(ge,tt),[()=>_("mcp.noTools")]),v(Q,ye)};z(be,Q=>{a(s)===a(P).name&&a(P).tools&&a(P).tools.length>0?Q(Ne):a(s)===a(P).name&&(!a(P).tools||a(P).tools.length===0)&&Q(ie,1)})}M((Q,ye)=>{var Ve;b(B,a(P).name),Ye(K,1,`rounded-full px-2 py-1 text-xs font-medium ${a(P).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),b(J,Q),b(Ie,a(P).url),b(W,`${((Ve=a(P).tools)==null?void 0:Ve.length)??0??""} ${ye??""}`)},[()=>a(P).status==="connected"?_("mcp.connected"):_("mcp.disconnected"),()=>_("mcp.tools")]),G("click",Se,()=>d(a(P).name)),v($,U)}),v(A,m)};z(j,A=>{a(n)?A(N):a(o)?A(I,1):a(r).length===0?A(X,2):A(q,-1)})}M((A,m)=>{b(w,A),b(T,m)},[()=>_("mcp.title"),()=>_("common.refresh")]),G("click",L,c),v(e,f),Oe()}ir(["click"]);var r0=x('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),a0=x("<div> </div>"),n0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),o0=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),s0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),i0=x('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),l0=x('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),d0=x('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),c0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <button type="button"><span></span></button></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between"><span> </span> <!></div></article>'),u0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),f0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),v0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),g0=x('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),p0=x('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),b0=x('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),y0=x('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),h0=x('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),m0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),_0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),x0=x('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option><option>ClawHub</option><option>HuggingFace</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),k0=x('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function w0(e,t){Pe(t,!0);let r=R("installed"),n=R(mt([])),o=R(!0),s=R(""),l=R(""),d=R("success"),c=R(mt([])),f=R(!1),h=R(""),k=R("github"),w=R(!1),L=R(""),T=R(""),j=R("");function N(H,Y="success"){p(l,H,!0),p(d,Y,!0),setTimeout(()=>{p(l,"")},3e3)}async function I(){try{const H=await St.getSkills();p(n,Array.isArray(H==null?void 0:H.skills)?H.skills:[],!0),p(s,"")}catch{p(n,[],!0),p(s,"Failed to load skills.")}finally{p(o,!1)}}async function X(H){try{await St.toggleSkill(H),p(n,a(n).map(Y=>Y.name===H?{...Y,enabled:!Y.enabled}:Y),!0)}catch{p(n,a(n).map(Y=>Y.name===H?{...Y,enabled:!Y.enabled}:Y),!0)}}async function q(H){if(a(j)!==H){p(j,H,!0);return}p(j,""),p(T,H,!0);try{await St.uninstallSkill(H),p(n,a(n).filter(Y=>Y.name!==H),!0),N(_("skills.uninstallSuccess"))}catch(Y){N(_("skills.uninstallFailed")+(Y.message?`: ${Y.message}`:""),"error")}finally{p(T,"")}}const A=te(()=>[...a(n)].sort((H,Y)=>H.enabled===Y.enabled?0:H.enabled?-1:1)),m=te(()=>a(n).filter(H=>H.enabled).length);async function $(){!a(h).trim()&&a(k)==="github"&&p(h,"agent skill"),p(f,!0),p(w,!0);try{const H=await St.discoverSkills(a(k),a(h));p(c,Array.isArray(H==null?void 0:H.results)?H.results:[],!0)}catch{p(c,[],!0)}finally{p(f,!1)}}function P(H){return a(n).some(Y=>Y.name===H)}async function U(H,Y){p(L,H,!0);try{const he=await St.installSkill(H,Y);he!=null&&he.skill&&p(n,[...a(n),{...he.skill,enabled:!0}],!0),N(_("skills.installSuccess"))}catch(he){N(_("skills.installFailed")+(he.message?`: ${he.message}`:""),"error")}finally{p(L,"")}}function Se(H){H.key==="Enter"&&$()}Ft(()=>{I()});var me=k0(),Fe=i(me),Ge=i(Fe),B=i(Ge),K=i(B),J=g(B,2);{var ee=H=>{var Y=r0(),he=i(Y);M(nt=>b(he,`${a(m)??""}/${a(n).length??""} ${nt??""}`),[()=>_("skills.active")]),v(H,Y)};z(J,H=>{!a(o)&&a(n).length>0&&H(ee)})}var Ie=g(Ge,2),D=i(Ie),W=g(Fe,2),be=i(W),Ne=i(be),ie=g(be,2),Q=i(ie),ye=g(W,2);{var Ve=H=>{var Y=a0(),he=i(Y);M(()=>{Ye(Y,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),b(he,a(l))}),v(H,Y)};z(ye,H=>{a(l)&&H(Ve)})}var ge=g(ye,2);{var tt=H=>{var Y=He(),he=Ee(Y);{var nt=Re=>{var ze=n0(),st=i(ze);M(Ue=>b(st,Ue),[()=>_("skills.loading")]),v(Re,ze)},Ze=Re=>{var ze=o0(),st=i(ze);M(()=>b(st,a(s))),v(Re,ze)},ot=Re=>{var ze=s0(),st=i(ze);M(Ue=>b(st,Ue),[()=>_("skills.noSkills")]),v(Re,ze)},dt=Re=>{var ze=u0();Qe(ze,21,()=>a(A),rt,(st,Ue)=>{var _t=c0(),ue=i(_t),je=i(ue),Ae=i(je),Te=g(je,2),it=i(Te),re=g(ue,2);{var yt=O=>{var F=i0(),Z=i(F);M(()=>b(Z,a(Ue).description)),v(O,F)};z(re,O=>{a(Ue).description&&O(yt)})}var lt=g(re,2),xt=i(lt),kt=g(lt,2),S=i(kt),V=i(S),u=g(S,2);{var y=O=>{var F=l0(),Z=i(F),_e=i(Z),le=g(Z,2),ke=i(le),Ke=g(le,2),ne=i(Ke);M((oe,$e,se)=>{b(_e,oe),le.disabled=a(T)===a(Ue).name,b(ke,$e),b(ne,se)},[()=>_("skills.confirmUninstall").replace("{name}",a(Ue).name),()=>a(T)===a(Ue).name?_("skills.uninstalling"):_("common.yes"),()=>_("common.no")]),G("click",le,()=>q(a(Ue).name)),G("click",Ke,()=>{p(j,"")}),v(O,F)},C=O=>{var F=d0(),Z=i(F);M(_e=>b(Z,_e),[()=>_("skills.uninstall")]),G("click",F,()=>q(a(Ue).name)),v(O,F)};z(u,O=>{a(j)===a(Ue).name?O(y):O(C,-1)})}M(O=>{b(Ae,a(Ue).name),Ye(Te,1,`relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition ${a(Ue).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Ye(it,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(Ue).enabled?"translate-x-4":"translate-x-1"}`),b(xt,a(Ue).location),Ye(S,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Ue).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),b(V,O)},[()=>a(Ue).enabled?_("common.enabled"):_("common.disabled")]),G("click",Te,()=>X(a(Ue).name)),v(st,_t)}),v(Re,ze)};z(he,Re=>{a(o)?Re(nt):a(s)?Re(Ze,1):a(n).length===0?Re(ot,2):Re(dt,-1)})}v(H,Y)};z(ge,H=>{a(r)==="installed"&&H(tt)})}var at=g(ge,2);{var gt=H=>{var Y=x0(),he=Ee(Y),nt=i(he),Ze=i(nt);Ze.value=Ze.__value="github";var ot=g(Ze);ot.value=ot.__value="clawhub";var dt=g(ot);dt.value=dt.__value="huggingface";var Re=g(nt,2),ze=g(Re,2),st=i(ze),Ue=g(he,2);{var _t=Ae=>{var Te=f0(),it=i(Te);M(re=>b(it,re),[()=>_("skills.searching")]),v(Ae,Te)},ue=Ae=>{var Te=v0(),it=i(Te);M(re=>b(it,re),[()=>_("skills.noResults")]),v(Ae,Te)},je=Ae=>{var Te=_0();Qe(Te,21,()=>a(c),rt,(it,re)=>{const yt=te(()=>P(a(re).name));var lt=m0(),xt=i(lt),kt=i(xt),S=i(kt),V=i(S),u=g(S,2),y=i(u),C=g(kt,2),O=i(C),F=g(xt,2);{var Z=fe=>{var xe=g0(),et=i(xe);M(()=>b(et,a(re).description)),v(fe,xe)};z(F,fe=>{a(re).description&&fe(Z)})}var _e=g(F,2),le=i(_e);{var ke=fe=>{var xe=p0(),et=g(i(xe));M(()=>b(et,` ${a(re).stars??""}`)),v(fe,xe)};z(le,fe=>{a(re).stars>0&&fe(ke)})}var Ke=g(le,2);{var ne=fe=>{var xe=b0(),et=i(xe);M(()=>b(et,a(re).language)),v(fe,xe)};z(Ke,fe=>{a(re).language&&fe(ne)})}var oe=g(Ke,2),$e=i(oe),se=g(_e,2),ae=i(se),de=i(ae),ce=g(ae,2);{var pe=fe=>{var xe=y0(),et=i(xe);M(ht=>b(et,ht),[()=>_("skills.installed")]),v(fe,xe)},we=fe=>{var xe=h0(),et=i(xe);M(ht=>{xe.disabled=a(L)===a(re).url,b(et,ht)},[()=>a(L)===a(re).url?_("skills.installing"):_("skills.install")]),G("click",xe,()=>U(a(re).url,a(re).name)),v(fe,xe)};z(ce,fe=>{a(yt)?fe(pe):fe(we,-1)})}M((fe,xe,et)=>{b(V,a(re).name),b(y,`${fe??""} ${a(re).owner??""}`),b(O,a(re).source),Ye(oe,1,a(re).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400"),b($e,xe),ut(ae,"href",a(re).url),b(de,et)},[()=>_("skills.owner"),()=>a(re).has_license?_("skills.licensed"):_("skills.unlicensed"),()=>a(re).url.replace("https://github.com/","")]),v(it,lt)}),v(Ae,Te)};z(Ue,Ae=>{a(f)?Ae(_t):a(w)&&a(c).length===0?Ae(ue,1):a(c).length>0&&Ae(je,2)})}M((Ae,Te)=>{ut(Re,"placeholder",Ae),ze.disabled=a(f),b(st,Te)},[()=>_("skills.search"),()=>a(f)?_("skills.searching"):_("skills.searchBtn")]),Rn(nt,()=>a(k),Ae=>p(k,Ae)),G("keydown",Re,Se),Br(Re,()=>a(h),Ae=>p(h,Ae)),G("click",ze,$),v(H,Y)};z(at,H=>{a(r)==="discover"&&H(gt)})}M((H,Y,he,nt)=>{b(K,H),b(D,Y),Ye(be,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),b(Ne,he),Ye(ie,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),b(Q,nt)},[()=>_("skills.title"),()=>_("common.refresh"),()=>_("skills.tabInstalled"),()=>_("skills.tabDiscover")]),G("click",Ie,()=>{p(o,!0),I()}),G("click",be,()=>{p(r,"installed")}),G("click",ie,()=>{p(r,"discover")}),v(e,me),Oe()}ir(["click","keydown"]);var S0=x("<div> </div>"),A0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),E0=x('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),$0=x('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),C0=x('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),M0=x('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),N0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),T0=x('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),P0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),O0=x('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),I0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),L0=x('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function F0(e,t){Pe(t,!0);let r=R(mt([])),n=R(!0),o=R(""),s=R(""),l=R(""),d=R("success");function c(B,K="success"){p(l,B,!0),p(d,K,!0),setTimeout(()=>{p(l,"")},3e3)}async function f(){p(n,!0);try{const B=await St.getPlugins();p(r,Array.isArray(B==null?void 0:B.plugins)?B.plugins:[],!0),p(o,"")}catch{p(r,[],!0),p(o,_("plugins.loadFailed"),!0)}finally{p(n,!1)}}async function h(B){p(s,B,!0);try{await St.reloadPlugin(B),c(_("plugins.reloadSuccess",{name:B})),await f()}catch(K){c(_("plugins.reloadFailed")+(K.message?`: ${K.message}`:""),"error")}finally{p(s,"")}}function k(B){return typeof B=="string"&&B==="Active"?"text-green-500":typeof B=="object"&&(B!=null&&B.Error)?"text-red-500":"text-yellow-500"}function w(B){return typeof B=="string"&&B==="Active"?_("plugins.statusActive"):typeof B=="object"&&(B!=null&&B.Error)?B.Error:_("common.unknown")}Ft(()=>{f()});var L=L0(),T=Ee(L);{var j=B=>{var K=S0(),J=i(K);M(()=>{Ye(K,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),b(J,a(l))}),v(B,K)};z(T,B=>{a(l)&&B(j)})}var N=g(T,2),I=i(N),X=i(I),q=i(X);wo(q,{size:24});var A=g(q,2),m=i(A),$=g(X,2),P=i($),U=g(I,2);{var Se=B=>{var K=A0(),J=i(K);M(ee=>b(J,ee),[()=>_("plugins.loading")]),v(B,K)},me=B=>{var K=E0(),J=i(K);M(()=>b(J,a(o))),v(B,K)},Fe=B=>{var K=$0(),J=i(K);wo(J,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var ee=g(J,2),Ie=i(ee);M(D=>b(Ie,D),[()=>_("plugins.noPlugins")]),v(B,K)},Ge=B=>{var K=I0();Qe(K,21,()=>a(r),rt,(J,ee)=>{var Ie=O0(),D=i(Ie),W=i(D),be=i(W),Ne=i(be),ie=g(be,2),Q=i(ie),ye=g(W,2),Ve=i(ye);{var ge=ue=>{Ed(ue,{size:16})},tt=ue=>{Ad(ue,{size:16})};z(Ve,ue=>{typeof a(ee).status=="string"&&a(ee).status==="Active"?ue(ge):ue(tt,-1)})}var at=g(Ve,2),gt=i(at),H=g(D,2);{var Y=ue=>{var je=C0(),Ae=i(je);M(()=>b(Ae,a(ee).description)),v(ue,je)};z(H,ue=>{a(ee).description&&ue(Y)})}var he=g(H,2);{var nt=ue=>{var je=N0(),Ae=i(je),Te=i(Ae),it=g(Ae,2);Qe(it,21,()=>a(ee).capabilities,rt,(re,yt)=>{var lt=M0(),xt=i(lt);M(()=>b(xt,a(yt))),v(re,lt)}),M(re=>b(Te,re),[()=>_("plugins.capabilities")]),v(ue,je)};z(he,ue=>{var je;(je=a(ee).capabilities)!=null&&je.length&&ue(nt)})}var Ze=g(he,2);{var ot=ue=>{var je=P0(),Ae=i(je),Te=i(Ae),it=g(Ae,2);Qe(it,21,()=>a(ee).permissions_required,rt,(re,yt)=>{var lt=T0(),xt=i(lt);M(()=>b(xt,a(yt))),v(re,lt)}),M(re=>b(Te,re),[()=>_("plugins.permissions")]),v(ue,je)};z(Ze,ue=>{var je;(je=a(ee).permissions_required)!=null&&je.length&&ue(ot)})}var dt=g(Ze,2),Re=i(dt),ze=i(Re);{var st=ue=>{Id(ue,{size:14,class:"animate-spin"})},Ue=ue=>{Ps(ue,{size:14})};z(ze,ue=>{a(s)===a(ee).name?ue(st):ue(Ue,-1)})}var _t=g(ze);M((ue,je,Ae)=>{b(Ne,a(ee).name),b(Q,`v${a(ee).version??""}`),Ye(ye,1,`flex items-center gap-1 ${ue??""}`),b(gt,je),Re.disabled=a(s)===a(ee).name,b(_t,` ${Ae??""}`)},[()=>k(a(ee).status),()=>w(a(ee).status),()=>_("plugins.reload")]),G("click",Re,()=>h(a(ee).name)),v(J,Ie)}),v(B,K)};z(U,B=>{a(n)?B(Se):a(o)?B(me,1):a(r).length===0?B(Fe,2):B(Ge,-1)})}M((B,K)=>{b(m,B),b(P,K)},[()=>_("plugins.title"),()=>_("common.refresh")]),G("click",$,f),v(e,L),Oe()}ir(["click"]);var R0=x('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),j0=x('<button type="button"> </button>'),H0=x('<p class="px-2 py-1 text-xs text-gray-400 dark:text-gray-500">Loading...</p>'),D0=x('<div class="ml-4 mt-1 space-y-1 border-l border-gray-200 pl-3 dark:border-gray-700"><!> <!></div>'),z0=x('<button type="button"> </button> <!>',1),U0=x('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),B0=x('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),W0=x('<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function V0(e,t){Pe(t,!0);let r=R(mt($s())),n=R(mt(Ra())),o=R(!1),s=R(!0),l=R(mt([])),d=R(!1),c=R(mt(typeof window<"u"?window.location.hash:""));const f=te(()=>a(n).length>0),h=te(()=>a(f)&&a(r)==="/"?"/overview":a(r)),k=te(()=>a(h).startsWith("/chat/")?"/sessions":a(h)),w=te(()=>a(h)==="/config"),L=te(()=>a(c).startsWith("#config-section-")?a(c).slice(16):"");function T(J){try{return decodeURIComponent(J)}catch{return J}}const j=te(()=>a(h).startsWith("/chat/")?T(a(h).slice(6)):"");function N(){localStorage.getItem("prx-console-theme")==="light"?p(s,!1):p(s,!0),I()}function I(){a(s)?document.documentElement.classList.add("dark"):document.documentElement.classList.remove("dark")}function X(){p(s,!a(s)),localStorage.setItem("prx-console-theme",a(s)?"dark":"light"),I()}function q(){p(n,Ra(),!0)}function A(J){p(r,J,!0),p(o,!1),p(c,typeof window<"u"?window.location.hash:"",!0)}function m(J){p(n,J,!0),$r("/overview",!0)}function $(){Es(),p(n,""),$r("/",!0)}function P(J){$r(J)}function U(){p(c,window.location.hash,!0)}async function Se(){if(!(!a(f)||a(h)!=="/config"||a(d))){p(d,!0);try{const J=await St.getConfig();p(l,Dn(J),!0)}catch{p(l,Dn(null),!0)}finally{p(d,!1)}}}function me(J){Ms(J),p(o,!1)}Ft(()=>{N();const J=Il(A),ee=Ie=>{if(Ie.key==="prx-console-token"){q();return}if(Ie.key===un&&bd(),Ie.key==="prx-console-theme"){const D=localStorage.getItem("prx-console-theme");p(s,D!=="light"),I()}};return window.addEventListener("storage",ee),window.addEventListener("hashchange",U),()=>{J(),window.removeEventListener("storage",ee),window.removeEventListener("hashchange",U)}}),Ft(()=>{if(a(f)&&a(r)==="/"){$r("/overview",!0);return}!a(f)&&a(r)!=="/"&&$r("/",!0)}),Ft(()=>{if(a(w)){Se();return}p(l,[],!0)});var Fe=W0(),Ge=i(Fe);{var B=J=>{Vd(J,{onLogin:m})},K=J=>{var ee=B0(),Ie=i(ee);{var D=u=>{var y=R0();M(C=>ut(y,"aria-label",C),[()=>_("app.closeSidebar")]),G("click",y,()=>p(o,!1)),v(u,y)};z(Ie,u=>{a(o)&&u(D)})}var W=g(Ie,2),be=i(W),Ne=i(be),ie=i(Ne),Q=g(be,2);Qe(Q,21,()=>Pl,rt,(u,y)=>{var C=z0(),O=Ee(C),F=i(O),Z=g(O,2);{var _e=le=>{var ke=D0(),Ke=i(ke);Qe(Ke,17,()=>a(l),rt,($e,se)=>{var ae=j0(),de=i(ae);M(()=>{Ye(ae,1,`w-full rounded-md px-2 py-1.5 text-left text-xs transition ${a(L)===a(se).groupKey?"bg-sky-50 text-sky-700 dark:bg-sky-500/10 dark:text-sky-300":"text-gray-500 hover:bg-gray-100 hover:text-gray-800 dark:text-gray-400 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),b(de,a(se).label)}),G("click",ae,()=>me(a(se).groupKey)),v($e,ae)});var ne=g(Ke,2);{var oe=$e=>{var se=H0();v($e,se)};z(ne,$e=>{a(d)&&a(l).length===0&&$e(oe)})}v(le,ke)};z(Z,le=>{a(y).path==="/config"&&a(w)&&le(_e)})}M(le=>{Ye(O,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(k)===a(y).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),b(F,le)},[()=>_(a(y).labelKey)]),G("click",O,()=>P(a(y).path)),v(u,C)});var ye=g(W,2),Ve=i(ye),ge=i(Ve),tt=i(ge),at=i(tt),gt=g(tt,2),H=i(gt),Y=g(ge,2),he=i(Y),nt=i(he);{var Ze=u=>{zd(u,{size:16})},ot=u=>{Fd(u,{size:16})};z(nt,u=>{a(s)?u(Ze):u(ot,-1)})}var dt=g(he,2),Re=i(dt),ze=g(dt,2),st=i(ze),Ue=g(Ve,2),_t=i(Ue);{var ue=u=>{rc(u,{})},je=u=>{cc(u,{})},Ae=u=>{Ec(u,{get sessionId(){return a(j)}})},Te=te(()=>a(h).startsWith("/chat/")),it=u=>{Ic(u,{})},re=u=>{Wu(u,{})},yt=u=>{t0(u,{})},lt=u=>{w0(u,{})},xt=u=>{F0(u,{})},kt=u=>{$u(u,{})},S=u=>{Pu(u,{})},V=u=>{var y=U0(),C=i(y),O=i(C),F=g(C,2),Z=i(F);M((_e,le)=>{b(O,_e),b(Z,le)},[()=>_("app.notFound"),()=>_("app.backToOverview")]),G("click",F,()=>P("/overview")),v(u,y)};z(_t,u=>{a(h)==="/overview"?u(ue):a(h)==="/sessions"?u(je,1):a(Te)?u(Ae,2):a(h)==="/channels"?u(it,3):a(h)==="/hooks"?u(re,4):a(h)==="/mcp"?u(yt,5):a(h)==="/skills"?u(lt,6):a(h)==="/plugins"?u(xt,7):a(h)==="/config"?u(kt,8):a(h)==="/logs"?u(S,9):u(V,-1)})}M((u,y,C,O,F)=>{Ye(W,1,`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(o)?"translate-x-0":"-translate-x-full"}`),b(ie,u),b(at,y),b(H,C),ut(dt,"aria-label",O),b(Re,Qr.lang==="zh"?"中文 / EN":"EN / 中文"),b(st,F)},[()=>_("app.title"),()=>_("app.menu"),()=>_("app.title"),()=>_("app.language"),()=>_("common.logout")]),G("click",tt,()=>p(o,!a(o))),G("click",he,X),G("click",dt,function(...u){na==null||na.apply(this,u)}),G("click",ze,$),v(J,ee)};z(Ge,J=>{a(f)?J(K,-1):J(B)})}v(e,Fe),Oe()}ir(["click"]);il(V0,{target:document.getElementById("app")});
