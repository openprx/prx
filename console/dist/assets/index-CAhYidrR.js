var Mo=Object.defineProperty;var Gn=e=>{throw TypeError(e)};var Co=(e,t,r)=>t in e?Mo(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var Vt=(e,t,r)=>Co(e,typeof t!="symbol"?t+"":t,r),rn=(e,t,r)=>t.has(e)||Gn("Cannot "+r);var k=(e,t,r)=>(rn(e,t,"read from private field"),r?r.call(e):t.get(e)),Se=(e,t,r)=>t.has(e)?Gn("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),he=(e,t,r,a)=>(rn(e,t,"write to private field"),a?a.call(e,r):t.set(e,r),r),ct=(e,t,r)=>(rn(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const s of document.querySelectorAll('link[rel="modulepreload"]'))a(s);new MutationObserver(s=>{for(const o of s)if(o.type==="childList")for(const i of o.addedNodes)i.tagName==="LINK"&&i.rel==="modulepreload"&&a(i)}).observe(document,{childList:!0,subtree:!0});function r(s){const o={};return s.integrity&&(o.integrity=s.integrity),s.referrerPolicy&&(o.referrerPolicy=s.referrerPolicy),s.crossOrigin==="use-credentials"?o.credentials="include":s.crossOrigin==="anonymous"?o.credentials="omit":o.credentials="same-origin",o}function a(s){if(s.ep)return;s.ep=!0;const o=r(s);fetch(s.href,o)}})();const fn=!1;var Tn=Array.isArray,Po=Array.prototype.indexOf,aa=Array.prototype.includes,Wa=Array.from,To=Object.defineProperty,wr=Object.getOwnPropertyDescriptor,No=Object.getOwnPropertyDescriptors,Oo=Object.prototype,Io=Array.prototype,hs=Object.getPrototypeOf,Jn=Object.isExtensible;function ya(e){return typeof e=="function"}const Ge=()=>{};function Lo(e){for(var t=0;t<e.length;t++)e[t]()}function ys(){var e,t,r=new Promise((a,s)=>{e=a,t=s});return{promise:r,resolve:e,reject:t}}function vn(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const a of e)if(r.push(a),r.length===t)break;return r}const _t=2,ca=4,na=8,qa=1<<24,Cr=16,Yt=32,Wr=64,gn=128,Ut=512,ht=1024,yt=2048,Jt=4096,wt=8192,nr=16384,ua=32768,pr=65536,Yn=1<<17,Ro=1<<18,fa=1<<19,Fo=1<<20,rr=1<<25,Hr=65536,pn=1<<21,Nn=1<<22,Sr=1<<23,Ar=Symbol("$state"),ms=Symbol("legacy props"),Do=Symbol(""),Tr=new class extends Error{constructor(){super(...arguments);Vt(this,"name","StaleReactionError");Vt(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var gs;const On=!!((gs=globalThis.document)!=null&&gs.contentType)&&globalThis.document.contentType.includes("xml");function _s(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function jo(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function Uo(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function zo(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function Ho(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Bo(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Vo(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Wo(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function qo(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function Ko(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function Go(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function Jo(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const Yo=1,Xo=2,xs=4,Qo=8,Zo=16,ei=1,ti=4,ri=8,ai=16,ni=1,si=2,vt=Symbol(),ks="http://www.w3.org/1999/xhtml",ws="http://www.w3.org/2000/svg",oi="http://www.w3.org/1998/Math/MathML",ii="@attach";function li(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function di(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Ss(e){return e===this.v}function ci(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function As(e){return!ci(e,this.v)}let ui=!1,Ct=null;function sa(e){Ct=e}function _e(e,t=!1,r){Ct={p:Ct,i:!1,c:null,e:null,s:e,x:null,l:null}}function xe(e){var t=Ct,r=t.e;if(r!==null){t.e=null;for(var a of r)qs(a)}return t.i=!0,Ct=t.p,{}}function Es(){return!0}let Nr=[];function $s(){var e=Nr;Nr=[],Lo(e)}function sr(e){if(Nr.length===0&&!wa){var t=Nr;queueMicrotask(()=>{t===Nr&&$s()})}Nr.push(e)}function fi(){for(;Nr.length>0;)$s()}function Ms(e){var t=Te;if(t===null)return we.f|=Sr,e;if(!(t.f&ua)&&!(t.f&ca))throw e;kr(e,t)}function kr(e,t){for(;t!==null;){if(t.f&gn){if(!(t.f&ua))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const vi=-7169;function st(e,t){e.f=e.f&vi|t}function In(e){e.f&Ut||e.deps===null?st(e,ht):st(e,Jt)}function Cs(e){if(e!==null)for(const t of e)!(t.f&_t)||!(t.f&Hr)||(t.f^=Hr,Cs(t.deps))}function Ps(e,t,r){e.f&yt?t.add(e):e.f&Jt&&r.add(e),Cs(e.deps),st(e,ht)}const Ia=new Set;let ge=null,za=null,bt=null,Et=[],Ka=null,wa=!1,oa=null,gi=1;var mr,Jr,Rr,Yr,Xr,Qr,_r,Qt,Zr,Tt,bn,hn,yn,mn;const Wn=class Wn{constructor(){Se(this,Tt);Vt(this,"id",gi++);Vt(this,"current",new Map);Vt(this,"previous",new Map);Se(this,mr,new Set);Se(this,Jr,new Set);Se(this,Rr,0);Se(this,Yr,0);Se(this,Xr,null);Se(this,Qr,new Set);Se(this,_r,new Set);Se(this,Qt,new Map);Vt(this,"is_fork",!1);Se(this,Zr,!1)}skip_effect(t){k(this,Qt).has(t)||k(this,Qt).set(t,{d:[],m:[]})}unskip_effect(t){var r=k(this,Qt).get(t);if(r){k(this,Qt).delete(t);for(var a of r.d)st(a,yt),ar(a);for(a of r.m)st(a,Jt),ar(a)}}process(t){var s;Et=[],this.apply();var r=oa=[],a=[];for(const o of t)ct(this,Tt,hn).call(this,o,r,a);if(oa=null,ct(this,Tt,bn).call(this)){ct(this,Tt,yn).call(this,a),ct(this,Tt,yn).call(this,r);for(const[o,i]of k(this,Qt))Is(o,i)}else{za=this,ge=null;for(const o of k(this,mr))o(this);k(this,mr).clear(),k(this,Rr)===0&&ct(this,Tt,mn).call(this),Xn(a),Xn(r),k(this,Qr).clear(),k(this,_r).clear(),za=null,(s=k(this,Xr))==null||s.resolve()}bt=null}capture(t,r){r!==vt&&!this.previous.has(t)&&this.previous.set(t,r),t.f&Sr||(this.current.set(t,t.v),bt==null||bt.set(t,t.v))}activate(){ge=this,this.apply()}deactivate(){ge===this&&(ge=null,bt=null)}flush(){var t;if(Et.length>0)ge=this,Ts();else if(k(this,Rr)===0&&!this.is_fork){for(const r of k(this,mr))r(this);k(this,mr).clear(),ct(this,Tt,mn).call(this),(t=k(this,Xr))==null||t.resolve()}this.deactivate()}discard(){for(const t of k(this,Jr))t(this);k(this,Jr).clear()}increment(t){he(this,Rr,k(this,Rr)+1),t&&he(this,Yr,k(this,Yr)+1)}decrement(t){he(this,Rr,k(this,Rr)-1),t&&he(this,Yr,k(this,Yr)-1),!k(this,Zr)&&(he(this,Zr,!0),sr(()=>{he(this,Zr,!1),ct(this,Tt,bn).call(this)?Et.length>0&&this.flush():this.revive()}))}revive(){for(const t of k(this,Qr))k(this,_r).delete(t),st(t,yt),ar(t);for(const t of k(this,_r))st(t,Jt),ar(t);this.flush()}oncommit(t){k(this,mr).add(t)}ondiscard(t){k(this,Jr).add(t)}settled(){return(k(this,Xr)??he(this,Xr,ys())).promise}static ensure(){if(ge===null){const t=ge=new Wn;Ia.add(ge),wa||sr(()=>{ge===t&&t.flush()})}return ge}apply(){}};mr=new WeakMap,Jr=new WeakMap,Rr=new WeakMap,Yr=new WeakMap,Xr=new WeakMap,Qr=new WeakMap,_r=new WeakMap,Qt=new WeakMap,Zr=new WeakMap,Tt=new WeakSet,bn=function(){return this.is_fork||k(this,Yr)>0},hn=function(t,r,a){t.f^=ht;for(var s=t.first;s!==null;){var o=s.f,i=(o&(Yt|Wr))!==0,d=i&&(o&ht)!==0,c=(o&wt)!==0,u=d||k(this,Qt).has(s);if(!u&&s.fn!==null){i?c||(s.f^=ht):o&ca?r.push(s):o&(na|qa)&&c?a.push(s):Oa(s)&&(la(s),o&Cr&&(k(this,_r).add(s),c&&st(s,yt)));var p=s.first;if(p!==null){s=p;continue}}for(;s!==null;){var m=s.next;if(m!==null){s=m;break}s=s.parent}}},yn=function(t){for(var r=0;r<t.length;r+=1)Ps(t[r],k(this,Qr),k(this,_r))},mn=function(){var o;if(Ia.size>1){this.previous.clear();var t=ge,r=bt,a=!0;for(const i of Ia){if(i===this){a=!1;continue}const d=[];for(const[u,p]of this.current){if(i.current.has(u))if(a&&p!==i.current.get(u))i.current.set(u,p);else continue;d.push(u)}if(d.length===0)continue;const c=[...i.current.keys()].filter(u=>!this.current.has(u));if(c.length>0){var s=Et;Et=[];const u=new Set,p=new Map;for(const m of d)Ns(m,c,u,p);if(Et.length>0){ge=i,i.apply();for(const m of Et)ct(o=i,Tt,hn).call(o,m,[],[]);i.deactivate()}Et=s}}ge=t,bt=r}k(this,Qt).clear(),Ia.delete(this)};let Er=Wn;function pi(e){var t=wa;wa=!0;try{for(var r;;){if(fi(),Et.length===0&&(ge==null||ge.flush(),Et.length===0))return Ka=null,r;Ts()}}finally{wa=t}}function Ts(){var e=null;try{for(var t=0;Et.length>0;){var r=Er.ensure();if(t++>1e3){var a,s;bi()}r.process(Et),$r.clear()}}finally{Et=[],Ka=null,oa=null}}function bi(){try{Vo()}catch(e){kr(e,Ka)}}let Wt=null;function Xn(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var a=e[r++];if(!(a.f&(nr|wt))&&Oa(a)&&(Wt=new Set,la(a),a.deps===null&&a.first===null&&a.nodes===null&&a.teardown===null&&a.ac===null&&Ys(a),(Wt==null?void 0:Wt.size)>0)){$r.clear();for(const s of Wt){if(s.f&(nr|wt))continue;const o=[s];let i=s.parent;for(;i!==null;)Wt.has(i)&&(Wt.delete(i),o.push(i)),i=i.parent;for(let d=o.length-1;d>=0;d--){const c=o[d];c.f&(nr|wt)||la(c)}}Wt.clear()}}Wt=null}}function Ns(e,t,r,a){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const s of e.reactions){const o=s.f;o&_t?Ns(s,t,r,a):o&(Nn|Cr)&&!(o&yt)&&Os(s,t,a)&&(st(s,yt),ar(s))}}function Os(e,t,r){const a=r.get(e);if(a!==void 0)return a;if(e.deps!==null)for(const s of e.deps){if(aa.call(t,s))return!0;if(s.f&_t&&Os(s,t,r))return r.set(s,!0),!0}return r.set(e,!1),!1}function ar(e){var t=Ka=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(ca|na|qa)&&!(e.f&ua)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var a=t.f;if(oa!==null&&t===Te&&!(e.f&na))return;if(a&(Wr|Yt)){if(!(a&ht))return;t.f^=ht}}Et.push(t)}function Is(e,t){if(!(e.f&Yt&&e.f&ht)){e.f&yt?t.d.push(e):e.f&Jt&&t.m.push(e),st(e,ht);for(var r=e.first;r!==null;)Is(r,t),r=r.next}}function hi(e){let t=0,r=Br(0),a;return()=>{Fn()&&(n(r),Dn(()=>(t===0&&(a=ga(()=>e(()=>Sa(r)))),t+=1,()=>{sr(()=>{t-=1,t===0&&(a==null||a(),a=void 0,Sa(r))})})))}}var yi=pr|fa;function mi(e,t,r,a){new _i(e,t,r,a)}var jt,Pn,Zt,Fr,At,er,Rt,qt,ur,Dr,xr,ea,ta,ra,fr,Ba,ut,xi,ki,wi,_n,Da,ja,xn;class _i{constructor(t,r,a,s){Se(this,ut);Vt(this,"parent");Vt(this,"is_pending",!1);Vt(this,"transform_error");Se(this,jt);Se(this,Pn,null);Se(this,Zt);Se(this,Fr);Se(this,At);Se(this,er,null);Se(this,Rt,null);Se(this,qt,null);Se(this,ur,null);Se(this,Dr,0);Se(this,xr,0);Se(this,ea,!1);Se(this,ta,new Set);Se(this,ra,new Set);Se(this,fr,null);Se(this,Ba,hi(()=>(he(this,fr,Br(k(this,Dr))),()=>{he(this,fr,null)})));var o;he(this,jt,t),he(this,Zt,r),he(this,Fr,i=>{var d=Te;d.b=this,d.f|=gn,a(i)}),this.parent=Te.b,this.transform_error=s??((o=this.parent)==null?void 0:o.transform_error)??(i=>i),he(this,At,va(()=>{ct(this,ut,_n).call(this)},yi))}defer_effect(t){Ps(t,k(this,ta),k(this,ra))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!k(this,Zt).pending}update_pending_count(t){ct(this,ut,xn).call(this,t),he(this,Dr,k(this,Dr)+t),!(!k(this,fr)||k(this,ea))&&(he(this,ea,!0),sr(()=>{he(this,ea,!1),k(this,fr)&&ia(k(this,fr),k(this,Dr))}))}get_effect_pending(){return k(this,Ba).call(this),n(k(this,fr))}error(t){var r=k(this,Zt).onerror;let a=k(this,Zt).failed;if(!r&&!a)throw t;k(this,er)&&(mt(k(this,er)),he(this,er,null)),k(this,Rt)&&(mt(k(this,Rt)),he(this,Rt,null)),k(this,qt)&&(mt(k(this,qt)),he(this,qt,null));var s=!1,o=!1;const i=()=>{if(s){di();return}s=!0,o&&Jo(),k(this,qt)!==null&&Ur(k(this,qt),()=>{he(this,qt,null)}),ct(this,ut,ja).call(this,()=>{Er.ensure(),ct(this,ut,_n).call(this)})},d=c=>{try{o=!0,r==null||r(c,i),o=!1}catch(u){kr(u,k(this,At)&&k(this,At).parent)}a&&he(this,qt,ct(this,ut,ja).call(this,()=>{Er.ensure();try{return Mt(()=>{var u=Te;u.b=this,u.f|=gn,a(k(this,jt),()=>c,()=>i)})}catch(u){return kr(u,k(this,At).parent),null}}))};sr(()=>{var c;try{c=this.transform_error(t)}catch(u){kr(u,k(this,At)&&k(this,At).parent);return}c!==null&&typeof c=="object"&&typeof c.then=="function"?c.then(d,u=>kr(u,k(this,At)&&k(this,At).parent)):d(c)})}}jt=new WeakMap,Pn=new WeakMap,Zt=new WeakMap,Fr=new WeakMap,At=new WeakMap,er=new WeakMap,Rt=new WeakMap,qt=new WeakMap,ur=new WeakMap,Dr=new WeakMap,xr=new WeakMap,ea=new WeakMap,ta=new WeakMap,ra=new WeakMap,fr=new WeakMap,Ba=new WeakMap,ut=new WeakSet,xi=function(){try{he(this,er,Mt(()=>k(this,Fr).call(this,k(this,jt))))}catch(t){this.error(t)}},ki=function(t){const r=k(this,Zt).failed;r&&he(this,qt,Mt(()=>{r(k(this,jt),()=>t,()=>()=>{})}))},wi=function(){const t=k(this,Zt).pending;t&&(this.is_pending=!0,he(this,Rt,Mt(()=>t(k(this,jt)))),sr(()=>{var r=he(this,ur,document.createDocumentFragment()),a=vr();r.append(a),he(this,er,ct(this,ut,ja).call(this,()=>(Er.ensure(),Mt(()=>k(this,Fr).call(this,a))))),k(this,xr)===0&&(k(this,jt).before(r),he(this,ur,null),Ur(k(this,Rt),()=>{he(this,Rt,null)}),ct(this,ut,Da).call(this))}))},_n=function(){try{if(this.is_pending=this.has_pending_snippet(),he(this,xr,0),he(this,Dr,0),he(this,er,Mt(()=>{k(this,Fr).call(this,k(this,jt))})),k(this,xr)>0){var t=he(this,ur,document.createDocumentFragment());zn(k(this,er),t);const r=k(this,Zt).pending;he(this,Rt,Mt(()=>r(k(this,jt))))}else ct(this,ut,Da).call(this)}catch(r){this.error(r)}},Da=function(){this.is_pending=!1;for(const t of k(this,ta))st(t,yt),ar(t);for(const t of k(this,ra))st(t,Jt),ar(t);k(this,ta).clear(),k(this,ra).clear()},ja=function(t){var r=Te,a=we,s=Ct;or(k(this,At)),Ht(k(this,At)),sa(k(this,At).ctx);try{return t()}catch(o){return Ms(o),null}finally{or(r),Ht(a),sa(s)}},xn=function(t){var r;if(!this.has_pending_snippet()){this.parent&&ct(r=this.parent,ut,xn).call(r,t);return}he(this,xr,k(this,xr)+t),k(this,xr)===0&&(ct(this,ut,Da).call(this),k(this,Rt)&&Ur(k(this,Rt),()=>{he(this,Rt,null)}),k(this,ur)&&(k(this,jt).before(k(this,ur)),he(this,ur,null)))};function Ls(e,t,r,a){const s=Ga;var o=e.filter(m=>!m.settled);if(r.length===0&&o.length===0){a(t.map(s));return}var i=Te,d=Si(),c=o.length===1?o[0].promise:o.length>1?Promise.all(o.map(m=>m.promise)):null;function u(m){d();try{a(m)}catch(x){i.f&nr||kr(x,i)}kn()}if(r.length===0){c.then(()=>u(t.map(s)));return}function p(){d(),Promise.all(r.map(m=>Ei(m))).then(m=>u([...t.map(s),...m])).catch(m=>kr(m,i))}c?c.then(p):p()}function Si(){var e=Te,t=we,r=Ct,a=ge;return function(o=!0){or(e),Ht(t),sa(r),o&&(a==null||a.activate())}}function kn(e=!0){or(null),Ht(null),sa(null),e&&(ge==null||ge.deactivate())}function Ai(){var e=Te.b,t=ge,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function Ga(e){var t=_t|yt,r=we!==null&&we.f&_t?we:null;return Te!==null&&(Te.f|=fa),{ctx:Ct,deps:null,effects:null,equals:Ss,f:t,fn:e,reactions:null,rv:0,v:vt,wv:0,parent:r??Te,ac:null}}function Ei(e,t,r){Te===null&&jo();var s=void 0,o=Br(vt),i=!we,d=new Map;return Ui(()=>{var x;var c=ys();s=c.promise;try{Promise.resolve(e()).then(c.resolve,c.reject).finally(kn)}catch(N){c.reject(N),kn()}var u=ge;if(i){var p=Ai();(x=d.get(u))==null||x.reject(Tr),d.delete(u),d.set(u,c)}const m=(N,C=void 0)=>{if(u.activate(),C)C!==Tr&&(o.f|=Sr,ia(o,C));else{o.f&Sr&&(o.f^=Sr),ia(o,N);for(const[F,$]of d){if(d.delete(F),F===u)break;$.reject(Tr)}}p&&p()};c.promise.then(m,N=>m(null,N||"unknown"))}),Ya(()=>{for(const c of d.values())c.reject(Tr)}),new Promise(c=>{function u(p){function m(){p===s?c(o):u(s)}p.then(m,m)}u(s)})}function Ve(e){const t=Ga(e);return Zs(t),t}function Rs(e){const t=Ga(e);return t.equals=As,t}function $i(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)mt(t[r])}}function Mi(e){for(var t=e.parent;t!==null;){if(!(t.f&_t))return t.f&nr?null:t;t=t.parent}return null}function Ln(e){var t,r=Te;or(Mi(e));try{e.f&=~Hr,$i(e),t=ao(e)}finally{or(r)}return t}function Fs(e){var t=Ln(e);if(!e.equals(t)&&(e.wv=to(),(!(ge!=null&&ge.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){st(e,ht);return}Mr||(bt!==null?(Fn()||ge!=null&&ge.is_fork)&&bt.set(e,t):In(e))}function Ci(e){var t,r;if(e.effects!==null)for(const a of e.effects)(a.teardown||a.ac)&&((t=a.teardown)==null||t.call(a),(r=a.ac)==null||r.abort(Tr),a.teardown=Ge,a.ac=null,Ea(a,0),jn(a))}function Ds(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&la(t)}let wn=new Set;const $r=new Map;let js=!1;function Br(e,t){var r={f:0,v:e,reactions:null,equals:Ss,rv:0,wv:0};return r}function O(e,t){const r=Br(e);return Zs(r),r}function Pi(e,t=!1,r=!0){const a=Br(e);return t||(a.equals=As),a}function f(e,t,r=!1){we!==null&&(!Gt||we.f&Yn)&&Es()&&we.f&(_t|Cr|Nn|Yn)&&(zt===null||!aa.call(zt,e))&&Go();let a=r?ot(t):t;return ia(e,a)}function ia(e,t){if(!e.equals(t)){var r=e.v;Mr?$r.set(e,t):$r.set(e,r),e.v=t;var a=Er.ensure();if(a.capture(e,r),e.f&_t){const s=e;e.f&yt&&Ln(s),In(s)}e.wv=to(),Us(e,yt),Te!==null&&Te.f&ht&&!(Te.f&(Yt|Wr))&&(Dt===null?Hi([e]):Dt.push(e)),!a.is_fork&&wn.size>0&&!js&&Ti()}return t}function Ti(){js=!1;for(const e of wn)e.f&ht&&st(e,Jt),Oa(e)&&la(e);wn.clear()}function Sa(e){f(e,e.v+1)}function Us(e,t){var r=e.reactions;if(r!==null)for(var a=r.length,s=0;s<a;s++){var o=r[s],i=o.f,d=(i&yt)===0;if(d&&st(o,t),i&_t){var c=o;bt==null||bt.delete(c),i&Hr||(i&Ut&&(o.f|=Hr),Us(c,Jt))}else d&&(i&Cr&&Wt!==null&&Wt.add(o),ar(o))}}function ot(e){if(typeof e!="object"||e===null||Ar in e)return e;const t=hs(e);if(t!==Oo&&t!==Io)return e;var r=new Map,a=Tn(e),s=O(0),o=zr,i=d=>{if(zr===o)return d();var c=we,u=zr;Ht(null),rs(o);var p=d();return Ht(c),rs(u),p};return a&&r.set("length",O(e.length)),new Proxy(e,{defineProperty(d,c,u){(!("value"in u)||u.configurable===!1||u.enumerable===!1||u.writable===!1)&&qo();var p=r.get(c);return p===void 0?i(()=>{var m=O(u.value);return r.set(c,m),m}):f(p,u.value,!0),!0},deleteProperty(d,c){var u=r.get(c);if(u===void 0){if(c in d){const p=i(()=>O(vt));r.set(c,p),Sa(s)}}else f(u,vt),Sa(s);return!0},get(d,c,u){var N;if(c===Ar)return e;var p=r.get(c),m=c in d;if(p===void 0&&(!m||(N=wr(d,c))!=null&&N.writable)&&(p=i(()=>{var C=ot(m?d[c]:vt),F=O(C);return F}),r.set(c,p)),p!==void 0){var x=n(p);return x===vt?void 0:x}return Reflect.get(d,c,u)},getOwnPropertyDescriptor(d,c){var u=Reflect.getOwnPropertyDescriptor(d,c);if(u&&"value"in u){var p=r.get(c);p&&(u.value=n(p))}else if(u===void 0){var m=r.get(c),x=m==null?void 0:m.v;if(m!==void 0&&x!==vt)return{enumerable:!0,configurable:!0,value:x,writable:!0}}return u},has(d,c){var x;if(c===Ar)return!0;var u=r.get(c),p=u!==void 0&&u.v!==vt||Reflect.has(d,c);if(u!==void 0||Te!==null&&(!p||(x=wr(d,c))!=null&&x.writable)){u===void 0&&(u=i(()=>{var N=p?ot(d[c]):vt,C=O(N);return C}),r.set(c,u));var m=n(u);if(m===vt)return!1}return p},set(d,c,u,p){var V;var m=r.get(c),x=c in d;if(a&&c==="length")for(var N=u;N<m.v;N+=1){var C=r.get(N+"");C!==void 0?f(C,vt):N in d&&(C=i(()=>O(vt)),r.set(N+"",C))}if(m===void 0)(!x||(V=wr(d,c))!=null&&V.writable)&&(m=i(()=>O(void 0)),f(m,ot(u)),r.set(c,m));else{x=m.v!==vt;var F=i(()=>ot(u));f(m,F)}var $=Reflect.getOwnPropertyDescriptor(d,c);if($!=null&&$.set&&$.set.call(p,u),!x){if(a&&typeof c=="string"){var P=r.get("length"),X=Number(c);Number.isInteger(X)&&X>=P.v&&f(P,X+1)}Sa(s)}return!0},ownKeys(d){n(s);var c=Reflect.ownKeys(d).filter(m=>{var x=r.get(m);return x===void 0||x.v!==vt});for(var[u,p]of r)p.v!==vt&&!(u in d)&&c.push(u);return c},setPrototypeOf(){Ko()}})}function Qn(e){try{if(e!==null&&typeof e=="object"&&Ar in e)return e[Ar]}catch{}return e}function Ni(e,t){return Object.is(Qn(e),Qn(t))}var Zn,zs,Hs,Bs;function Oi(){if(Zn===void 0){Zn=window,zs=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;Hs=wr(t,"firstChild").get,Bs=wr(t,"nextSibling").get,Jn(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),Jn(r)&&(r.__t=void 0)}}function vr(e=""){return document.createTextNode(e)}function gr(e){return Hs.call(e)}function Na(e){return Bs.call(e)}function l(e,t){return gr(e)}function Ne(e,t=!1){{var r=gr(e);return r instanceof Comment&&r.data===""?Na(r):r}}function b(e,t=1,r=!1){let a=e;for(;t--;)a=Na(a);return a}function Ii(e){e.textContent=""}function Vs(){return!1}function Rn(e,t,r){return document.createElementNS(t??ks,e,void 0)}function Li(e,t){if(t){const r=document.body;e.autofocus=!0,sr(()=>{document.activeElement===r&&e.focus()})}}let es=!1;function Ri(){es||(es=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function Ja(e){var t=we,r=Te;Ht(null),or(null);try{return e()}finally{Ht(t),or(r)}}function Ws(e,t,r,a=r){e.addEventListener(t,()=>Ja(r));const s=e.__on_r;s?e.__on_r=()=>{s(),a(!0)}:e.__on_r=()=>a(!0),Ri()}function Fi(e){Te===null&&(we===null&&Bo(),Ho()),Mr&&zo()}function Di(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function ir(e,t){var r=Te;r!==null&&r.f&wt&&(e|=wt);var a={ctx:Ct,deps:null,nodes:null,f:e|yt|Ut,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},s=a;if(e&ca)oa!==null?oa.push(a):ar(a);else if(t!==null){try{la(a)}catch(i){throw mt(a),i}s.deps===null&&s.teardown===null&&s.nodes===null&&s.first===s.last&&!(s.f&fa)&&(s=s.first,e&Cr&&e&pr&&s!==null&&(s.f|=pr))}if(s!==null&&(s.parent=r,r!==null&&Di(s,r),we!==null&&we.f&_t&&!(e&Wr))){var o=we;(o.effects??(o.effects=[])).push(s)}return a}function Fn(){return we!==null&&!Gt}function Ya(e){const t=ir(na,null);return st(t,ht),t.teardown=e,t}function Pt(e){Fi();var t=Te.f,r=!we&&(t&Yt)!==0&&(t&ua)===0;if(r){var a=Ct;(a.e??(a.e=[])).push(e)}else return qs(e)}function qs(e){return ir(ca|Fo,e)}function ji(e){Er.ensure();const t=ir(Wr|fa,e);return(r={})=>new Promise(a=>{r.outro?Ur(t,()=>{mt(t),a(void 0)}):(mt(t),a(void 0))})}function Xa(e){return ir(ca,e)}function Ui(e){return ir(Nn|fa,e)}function Dn(e,t=0){return ir(na|t,e)}function M(e,t=[],r=[],a=[]){Ls(a,t,r,s=>{ir(na,()=>e(...s.map(n)))})}function va(e,t=0){var r=ir(Cr|t,e);return r}function Ks(e,t=0){var r=ir(qa|t,e);return r}function Mt(e){return ir(Yt|fa,e)}function Gs(e){var t=e.teardown;if(t!==null){const r=Mr,a=we;ts(!0),Ht(null);try{t.call(null)}finally{ts(r),Ht(a)}}}function jn(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const s=r.ac;s!==null&&Ja(()=>{s.abort(Tr)});var a=r.next;r.f&Wr?r.parent=null:mt(r,t),r=a}}function zi(e){for(var t=e.first;t!==null;){var r=t.next;t.f&Yt||mt(t),t=r}}function mt(e,t=!0){var r=!1;(t||e.f&Ro)&&e.nodes!==null&&e.nodes.end!==null&&(Js(e.nodes.start,e.nodes.end),r=!0),jn(e,t&&!r),Ea(e,0),st(e,nr);var a=e.nodes&&e.nodes.t;if(a!==null)for(const o of a)o.stop();Gs(e);var s=e.parent;s!==null&&s.first!==null&&Ys(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function Js(e,t){for(;e!==null;){var r=e===t?null:Na(e);e.remove(),e=r}}function Ys(e){var t=e.parent,r=e.prev,a=e.next;r!==null&&(r.next=a),a!==null&&(a.prev=r),t!==null&&(t.first===e&&(t.first=a),t.last===e&&(t.last=r))}function Ur(e,t,r=!0){var a=[];Xs(e,a,!0);var s=()=>{r&&mt(e),t&&t()},o=a.length;if(o>0){var i=()=>--o||s();for(var d of a)d.out(i)}else s()}function Xs(e,t,r){if(!(e.f&wt)){e.f^=wt;var a=e.nodes&&e.nodes.t;if(a!==null)for(const d of a)(d.is_global||r)&&t.push(d);for(var s=e.first;s!==null;){var o=s.next,i=(s.f&pr)!==0||(s.f&Yt)!==0&&(e.f&Cr)!==0;Xs(s,t,i?r:!1),s=o}}}function Un(e){Qs(e,!0)}function Qs(e,t){if(e.f&wt){e.f^=wt;for(var r=e.first;r!==null;){var a=r.next,s=(r.f&pr)!==0||(r.f&Yt)!==0;Qs(r,s?t:!1),r=a}var o=e.nodes&&e.nodes.t;if(o!==null)for(const i of o)(i.is_global||t)&&i.in()}}function zn(e,t){if(e.nodes)for(var r=e.nodes.start,a=e.nodes.end;r!==null;){var s=r===a?null:Na(r);t.append(r),r=s}}let Ua=!1,Mr=!1;function ts(e){Mr=e}let we=null,Gt=!1;function Ht(e){we=e}let Te=null;function or(e){Te=e}let zt=null;function Zs(e){we!==null&&(zt===null?zt=[e]:zt.push(e))}let $t=null,Lt=0,Dt=null;function Hi(e){Dt=e}let eo=1,Or=0,zr=Or;function rs(e){zr=e}function to(){return++eo}function Oa(e){var t=e.f;if(t&yt)return!0;if(t&_t&&(e.f&=~Hr),t&Jt){for(var r=e.deps,a=r.length,s=0;s<a;s++){var o=r[s];if(Oa(o)&&Fs(o),o.wv>e.wv)return!0}t&Ut&&bt===null&&st(e,ht)}return!1}function ro(e,t,r=!0){var a=e.reactions;if(a!==null&&!(zt!==null&&aa.call(zt,e)))for(var s=0;s<a.length;s++){var o=a[s];o.f&_t?ro(o,t,!1):t===o&&(r?st(o,yt):o.f&ht&&st(o,Jt),ar(o))}}function ao(e){var F;var t=$t,r=Lt,a=Dt,s=we,o=zt,i=Ct,d=Gt,c=zr,u=e.f;$t=null,Lt=0,Dt=null,we=u&(Yt|Wr)?null:e,zt=null,sa(e.ctx),Gt=!1,zr=++Or,e.ac!==null&&(Ja(()=>{e.ac.abort(Tr)}),e.ac=null);try{e.f|=pn;var p=e.fn,m=p();e.f|=ua;var x=e.deps,N=ge==null?void 0:ge.is_fork;if($t!==null){var C;if(N||Ea(e,Lt),x!==null&&Lt>0)for(x.length=Lt+$t.length,C=0;C<$t.length;C++)x[Lt+C]=$t[C];else e.deps=x=$t;if(Fn()&&e.f&Ut)for(C=Lt;C<x.length;C++)((F=x[C]).reactions??(F.reactions=[])).push(e)}else!N&&x!==null&&Lt<x.length&&(Ea(e,Lt),x.length=Lt);if(Es()&&Dt!==null&&!Gt&&x!==null&&!(e.f&(_t|Jt|yt)))for(C=0;C<Dt.length;C++)ro(Dt[C],e);if(s!==null&&s!==e){if(Or++,s.deps!==null)for(let $=0;$<r;$+=1)s.deps[$].rv=Or;if(t!==null)for(const $ of t)$.rv=Or;Dt!==null&&(a===null?a=Dt:a.push(...Dt))}return e.f&Sr&&(e.f^=Sr),m}catch($){return Ms($)}finally{e.f^=pn,$t=t,Lt=r,Dt=a,we=s,zt=o,sa(i),Gt=d,zr=c}}function Bi(e,t){let r=t.reactions;if(r!==null){var a=Po.call(r,e);if(a!==-1){var s=r.length-1;s===0?r=t.reactions=null:(r[a]=r[s],r.pop())}}if(r===null&&t.f&_t&&($t===null||!aa.call($t,t))){var o=t;o.f&Ut&&(o.f^=Ut,o.f&=~Hr),In(o),Ci(o),Ea(o,0)}}function Ea(e,t){var r=e.deps;if(r!==null)for(var a=t;a<r.length;a++)Bi(e,r[a])}function la(e){var t=e.f;if(!(t&nr)){st(e,ht);var r=Te,a=Ua;Te=e,Ua=!0;try{t&(Cr|qa)?zi(e):jn(e),Gs(e);var s=ao(e);e.teardown=typeof s=="function"?s:null,e.wv=eo;var o;fn&&ui&&e.f&yt&&e.deps}finally{Ua=a,Te=r}}}async function no(){await Promise.resolve(),pi()}function n(e){var t=e.f,r=(t&_t)!==0;if(we!==null&&!Gt){var a=Te!==null&&(Te.f&nr)!==0;if(!a&&(zt===null||!aa.call(zt,e))){var s=we.deps;if(we.f&pn)e.rv<Or&&(e.rv=Or,$t===null&&s!==null&&s[Lt]===e?Lt++:$t===null?$t=[e]:$t.push(e));else{(we.deps??(we.deps=[])).push(e);var o=e.reactions;o===null?e.reactions=[we]:aa.call(o,we)||o.push(we)}}}if(Mr&&$r.has(e))return $r.get(e);if(r){var i=e;if(Mr){var d=i.v;return(!(i.f&ht)&&i.reactions!==null||oo(i))&&(d=Ln(i)),$r.set(i,d),d}var c=(i.f&Ut)===0&&!Gt&&we!==null&&(Ua||(we.f&Ut)!==0),u=(i.f&ua)===0;Oa(i)&&(c&&(i.f|=Ut),Fs(i)),c&&!u&&(Ds(i),so(i))}if(bt!=null&&bt.has(e))return bt.get(e);if(e.f&Sr)throw e.v;return e.v}function so(e){if(e.f|=Ut,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&_t&&!(t.f&Ut)&&(Ds(t),so(t))}function oo(e){if(e.v===vt)return!0;if(e.deps===null)return!1;for(const t of e.deps)if($r.has(t)||t.f&_t&&oo(t))return!0;return!1}function ga(e){var t=Gt;try{return Gt=!0,e()}finally{Gt=t}}function Vi(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Wi=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function qi(e){return Wi.includes(e)}const Ki={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function Gi(e){return e=e.toLowerCase(),Ki[e]??e}const Ji=["touchstart","touchmove"];function Yi(e){return Ji.includes(e)}const Ir=Symbol("events"),io=new Set,Sn=new Set;function lo(e,t,r,a={}){function s(o){if(a.capture||An.call(t,o),!o.cancelBubble)return Ja(()=>r==null?void 0:r.call(this,o))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?sr(()=>{t.addEventListener(e,s,a)}):t.addEventListener(e,s,a),s}function qr(e,t,r,a,s){var o={capture:a,passive:s},i=lo(e,t,r,o);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&Ya(()=>{t.removeEventListener(e,i,o)})}function ae(e,t,r){(t[Ir]??(t[Ir]={}))[e]=r}function Xt(e){for(var t=0;t<e.length;t++)io.add(e[t]);for(var r of Sn)r(e)}let as=null;function An(e){var $,P;var t=this,r=t.ownerDocument,a=e.type,s=(($=e.composedPath)==null?void 0:$.call(e))||[],o=s[0]||e.target;as=e;var i=0,d=as===e&&e[Ir];if(d){var c=s.indexOf(d);if(c!==-1&&(t===document||t===window)){e[Ir]=t;return}var u=s.indexOf(t);if(u===-1)return;c<=u&&(i=c)}if(o=s[i]||e.target,o!==t){To(e,"currentTarget",{configurable:!0,get(){return o||r}});var p=we,m=Te;Ht(null),or(null);try{for(var x,N=[];o!==null;){var C=o.assignedSlot||o.parentNode||o.host||null;try{var F=(P=o[Ir])==null?void 0:P[a];F!=null&&(!o.disabled||e.target===o)&&F.call(o,e)}catch(X){x?N.push(X):x=X}if(e.cancelBubble||C===t||C===null)break;o=C}if(x){for(let X of N)queueMicrotask(()=>{throw X});throw x}}finally{e[Ir]=t,delete e.currentTarget,Ht(p),or(m)}}}var ps;const an=((ps=globalThis==null?void 0:globalThis.window)==null?void 0:ps.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function Xi(e){return(an==null?void 0:an.createHTML(e))??e}function co(e){var t=Rn("template");return t.innerHTML=Xi(e.replaceAll("<!>","<!---->")),t.content}function da(e,t){var r=Te;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function A(e,t){var r=(t&ni)!==0,a=(t&si)!==0,s,o=!e.startsWith("<!>");return()=>{s===void 0&&(s=co(o?e:"<!>"+e),r||(s=gr(s)));var i=a||zs?document.importNode(s,!0):s.cloneNode(!0);if(r){var d=gr(i),c=i.lastChild;da(d,c)}else da(i,i);return i}}function Qi(e,t,r="svg"){var a=!e.startsWith("<!>"),s=`<${r}>${a?e:"<!>"+e}</${r}>`,o;return()=>{if(!o){var i=co(s),d=gr(i);o=gr(d)}var c=o.cloneNode(!0);return da(c,c),c}}function Zi(e,t){return Qi(e,t,"svg")}function De(){var e=document.createDocumentFragment(),t=document.createComment(""),r=vr();return e.append(t,r),da(t,r),e}function h(e,t){e!==null&&e.before(t)}function g(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function el(e,t){return tl(e,t)}const La=new Map;function tl(e,{target:t,anchor:r,props:a={},events:s,context:o,intro:i=!0,transformError:d}){Oi();var c=void 0,u=ji(()=>{var p=r??t.appendChild(vr());mi(p,{pending:()=>{}},N=>{_e({});var C=Ct;o&&(C.c=o),s&&(a.$$events=s),c=e(N,a)||{},xe()},d);var m=new Set,x=N=>{for(var C=0;C<N.length;C++){var F=N[C];if(!m.has(F)){m.add(F);var $=Yi(F);for(const V of[t,document]){var P=La.get(V);P===void 0&&(P=new Map,La.set(V,P));var X=P.get(F);X===void 0?(V.addEventListener(F,An,{passive:$}),P.set(F,1)):P.set(F,X+1)}}}};return x(Wa(io)),Sn.add(x),()=>{var $;for(var N of m)for(const P of[t,document]){var C=La.get(P),F=C.get(N);--F==0?(P.removeEventListener(N,An),C.delete(N),C.size===0&&La.delete(P)):C.set(N,F)}Sn.delete(x),p!==r&&(($=p.parentNode)==null||$.removeChild(p))}});return rl.set(c,u),c}let rl=new WeakMap;var Kt,tr,Ft,jr,Pa,Ta,Va;class Qa{constructor(t,r=!0){Vt(this,"anchor");Se(this,Kt,new Map);Se(this,tr,new Map);Se(this,Ft,new Map);Se(this,jr,new Set);Se(this,Pa,!0);Se(this,Ta,t=>{if(k(this,Kt).has(t)){var r=k(this,Kt).get(t),a=k(this,tr).get(r);if(a)Un(a),k(this,jr).delete(r);else{var s=k(this,Ft).get(r);s&&!(s.effect.f&wt)&&(k(this,tr).set(r,s.effect),k(this,Ft).delete(r),s.fragment.lastChild.remove(),this.anchor.before(s.fragment),a=s.effect)}for(const[o,i]of k(this,Kt)){if(k(this,Kt).delete(o),o===t)break;const d=k(this,Ft).get(i);d&&(mt(d.effect),k(this,Ft).delete(i))}for(const[o,i]of k(this,tr)){if(o===r||k(this,jr).has(o)||i.f&wt)continue;const d=()=>{if(Array.from(k(this,Kt).values()).includes(o)){var u=document.createDocumentFragment();zn(i,u),u.append(vr()),k(this,Ft).set(o,{effect:i,fragment:u})}else mt(i);k(this,jr).delete(o),k(this,tr).delete(o)};k(this,Pa)||!a?(k(this,jr).add(o),Ur(i,d,!1)):d()}}});Se(this,Va,t=>{k(this,Kt).delete(t);const r=Array.from(k(this,Kt).values());for(const[a,s]of k(this,Ft))r.includes(a)||(mt(s.effect),k(this,Ft).delete(a))});this.anchor=t,he(this,Pa,r)}ensure(t,r){var a=ge,s=Vs();if(r&&!k(this,tr).has(t)&&!k(this,Ft).has(t))if(s){var o=document.createDocumentFragment(),i=vr();o.append(i),k(this,Ft).set(t,{effect:Mt(()=>r(i)),fragment:o})}else k(this,tr).set(t,Mt(()=>r(this.anchor)));if(k(this,Kt).set(a,t),s){for(const[d,c]of k(this,tr))d===t?a.unskip_effect(c):a.skip_effect(c);for(const[d,c]of k(this,Ft))d===t?a.unskip_effect(c.effect):a.skip_effect(c.effect);a.oncommit(k(this,Ta)),a.ondiscard(k(this,Va))}else k(this,Ta).call(this,a)}}Kt=new WeakMap,tr=new WeakMap,Ft=new WeakMap,jr=new WeakMap,Pa=new WeakMap,Ta=new WeakMap,Va=new WeakMap;function ne(e,t,r=!1){var a=new Qa(e),s=r?pr:0;function o(i,d){a.ensure(i,d)}va(()=>{var i=!1;t((d,c=0)=>{i=!0,o(c,d)}),i||o(-1,null)},s)}function rt(e,t){return t}function al(e,t,r){for(var a=[],s=t.length,o,i=t.length,d=0;d<s;d++){let m=t[d];Ur(m,()=>{if(o){if(o.pending.delete(m),o.done.add(m),o.pending.size===0){var x=e.outrogroups;En(e,Wa(o.done)),x.delete(o),x.size===0&&(e.outrogroups=null)}}else i-=1},!1)}if(i===0){var c=a.length===0&&r!==null;if(c){var u=r,p=u.parentNode;Ii(p),p.append(u),e.items.clear()}En(e,t,!c)}else o={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(o)}function En(e,t,r=!0){var a;if(e.pending.size>0){a=new Set;for(const i of e.pending.values())for(const d of i)a.add(e.items.get(d).e)}for(var s=0;s<t.length;s++){var o=t[s];if(a!=null&&a.has(o)){o.f|=rr;const i=document.createDocumentFragment();zn(o,i)}else mt(t[s],r)}}var ns;function Ke(e,t,r,a,s,o=null){var i=e,d=new Map,c=(t&xs)!==0;if(c){var u=e;i=u.appendChild(vr())}var p=null,m=Rs(()=>{var V=r();return Tn(V)?V:V==null?[]:Wa(V)}),x,N=new Map,C=!0;function F(V){X.effect.f&nr||(X.pending.delete(V),X.fallback=p,nl(X,x,i,t,a),p!==null&&(x.length===0?p.f&rr?(p.f^=rr,ka(p,null,i)):Un(p):Ur(p,()=>{p=null})))}function $(V){X.pending.delete(V)}var P=va(()=>{x=n(m);for(var V=x.length,w=new Set,v=ge,E=Vs(),S=0;S<V;S+=1){var j=x[S],re=a(j,S),ce=C?null:d.get(re);ce?(ce.v&&ia(ce.v,j),ce.i&&ia(ce.i,S),E&&v.unskip_effect(ce.e)):(ce=sl(d,C?i:ns??(ns=vr()),j,re,S,s,t,r),C||(ce.e.f|=rr),d.set(re,ce)),w.add(re)}if(V===0&&o&&!p&&(C?p=Mt(()=>o(i)):(p=Mt(()=>o(ns??(ns=vr()))),p.f|=rr)),V>w.size&&Uo(),!C)if(N.set(v,w),E){for(const[Ee,$e]of d)w.has(Ee)||v.skip_effect($e.e);v.oncommit(F),v.ondiscard($)}else F(v);n(m)}),X={effect:P,items:d,pending:N,outrogroups:null,fallback:p};C=!1}function ma(e){for(;e!==null&&!(e.f&Yt);)e=e.next;return e}function nl(e,t,r,a,s){var ce,Ee,$e,B,K,pe,se,Ae,U;var o=(a&Qo)!==0,i=t.length,d=e.items,c=ma(e.effect.first),u,p=null,m,x=[],N=[],C,F,$,P;if(o)for(P=0;P<i;P+=1)C=t[P],F=s(C,P),$=d.get(F).e,$.f&rr||((Ee=(ce=$.nodes)==null?void 0:ce.a)==null||Ee.measure(),(m??(m=new Set)).add($));for(P=0;P<i;P+=1){if(C=t[P],F=s(C,P),$=d.get(F).e,e.outrogroups!==null)for(const H of e.outrogroups)H.pending.delete($),H.done.delete($);if($.f&rr)if($.f^=rr,$===c)ka($,null,r);else{var X=p?p.next:c;$===e.effect.last&&(e.effect.last=$.prev),$.prev&&($.prev.next=$.next),$.next&&($.next.prev=$.prev),hr(e,p,$),hr(e,$,X),ka($,X,r),p=$,x=[],N=[],c=ma(p.next);continue}if($.f&wt&&(Un($),o&&((B=($e=$.nodes)==null?void 0:$e.a)==null||B.unfix(),(m??(m=new Set)).delete($))),$!==c){if(u!==void 0&&u.has($)){if(x.length<N.length){var V=N[0],w;p=V.prev;var v=x[0],E=x[x.length-1];for(w=0;w<x.length;w+=1)ka(x[w],V,r);for(w=0;w<N.length;w+=1)u.delete(N[w]);hr(e,v.prev,E.next),hr(e,p,v),hr(e,E,V),c=V,p=E,P-=1,x=[],N=[]}else u.delete($),ka($,c,r),hr(e,$.prev,$.next),hr(e,$,p===null?e.effect.first:p.next),hr(e,p,$),p=$;continue}for(x=[],N=[];c!==null&&c!==$;)(u??(u=new Set)).add(c),N.push(c),c=ma(c.next);if(c===null)continue}$.f&rr||x.push($),p=$,c=ma($.next)}if(e.outrogroups!==null){for(const H of e.outrogroups)H.pending.size===0&&(En(e,Wa(H.done)),(K=e.outrogroups)==null||K.delete(H));e.outrogroups.size===0&&(e.outrogroups=null)}if(c!==null||u!==void 0){var S=[];if(u!==void 0)for($ of u)$.f&wt||S.push($);for(;c!==null;)!(c.f&wt)&&c!==e.fallback&&S.push(c),c=ma(c.next);var j=S.length;if(j>0){var re=a&xs&&i===0?r:null;if(o){for(P=0;P<j;P+=1)(se=(pe=S[P].nodes)==null?void 0:pe.a)==null||se.measure();for(P=0;P<j;P+=1)(U=(Ae=S[P].nodes)==null?void 0:Ae.a)==null||U.fix()}al(e,S,re)}}o&&sr(()=>{var H,fe;if(m!==void 0)for($ of m)(fe=(H=$.nodes)==null?void 0:H.a)==null||fe.apply()})}function sl(e,t,r,a,s,o,i,d){var c=i&Yo?i&Zo?Br(r):Pi(r,!1,!1):null,u=i&Xo?Br(s):null;return{v:c,i:u,e:Mt(()=>(o(t,c??r,u??s,d),()=>{e.delete(a)}))}}function ka(e,t,r){if(e.nodes)for(var a=e.nodes.start,s=e.nodes.end,o=t&&!(t.f&rr)?t.nodes.start:r;a!==null;){var i=Na(a);if(o.before(a),a===s)return;a=i}}function hr(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function ol(e,t,r=!1,a=!1,s=!1){var o=e,i="";M(()=>{var d=Te;if(i!==(i=t()??"")&&(d.nodes!==null&&(Js(d.nodes.start,d.nodes.end),d.nodes=null),i!=="")){var c=r?ws:a?oi:void 0,u=Rn(r?"svg":a?"math":"template",c);u.innerHTML=i;var p=r||a?u:u.content;if(da(gr(p),p.lastChild),r||a)for(;gr(p);)o.before(gr(p));else o.before(p)}})}function Je(e,t,...r){var a=new Qa(e);va(()=>{const s=t()??null;a.ensure(s,s&&(o=>s(o,...r)))},pr)}function il(e,t,r){var a=new Qa(e);va(()=>{var s=t()??null;a.ensure(s,s&&(o=>r(o,s)))},pr)}function ll(e,t,r,a,s,o){var i=null,d=e,c=new Qa(d,!1);va(()=>{const u=t()||null;var p=ws;if(u===null){c.ensure(null,null);return}return c.ensure(u,m=>{if(u){if(i=Rn(u,p),da(i,i),a){var x=i.appendChild(vr());a(i,x)}Te.nodes.end=i,m.before(i)}}),()=>{}},pr),Ya(()=>{})}function dl(e,t){var r=void 0,a;Ks(()=>{r!==(r=t())&&(a&&(mt(a),a=null),r&&(a=Mt(()=>{Xa(()=>r(e))})))})}function uo(e){var t,r,a="";if(typeof e=="string"||typeof e=="number")a+=e;else if(typeof e=="object")if(Array.isArray(e)){var s=e.length;for(t=0;t<s;t++)e[t]&&(r=uo(e[t]))&&(a&&(a+=" "),a+=r)}else for(r in e)e[r]&&(a&&(a+=" "),a+=r);return a}function cl(){for(var e,t,r=0,a="",s=arguments.length;r<s;r++)(e=arguments[r])&&(t=uo(e))&&(a&&(a+=" "),a+=t);return a}function fo(e){return typeof e=="object"?cl(e):e??""}const ss=[...` 	
\r\f \v\uFEFF`];function ul(e,t,r){var a=e==null?"":""+e;if(r){for(var s of Object.keys(r))if(r[s])a=a?a+" "+s:s;else if(a.length)for(var o=s.length,i=0;(i=a.indexOf(s,i))>=0;){var d=i+o;(i===0||ss.includes(a[i-1]))&&(d===a.length||ss.includes(a[d]))?a=(i===0?"":a.substring(0,i))+a.substring(d+1):i=d}}return a===""?null:a}function os(e,t=!1){var r=t?" !important;":";",a="";for(var s of Object.keys(e)){var o=e[s];o!=null&&o!==""&&(a+=" "+s+": "+o+r)}return a}function nn(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function fl(e,t){if(t){var r="",a,s;if(Array.isArray(t)?(a=t[0],s=t[1]):a=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var o=!1,i=0,d=!1,c=[];a&&c.push(...Object.keys(a).map(nn)),s&&c.push(...Object.keys(s).map(nn));var u=0,p=-1;const F=e.length;for(var m=0;m<F;m++){var x=e[m];if(d?x==="/"&&e[m-1]==="*"&&(d=!1):o?o===x&&(o=!1):x==="/"&&e[m+1]==="*"?d=!0:x==='"'||x==="'"?o=x:x==="("?i++:x===")"&&i--,!d&&o===!1&&i===0){if(x===":"&&p===-1)p=m;else if(x===";"||m===F-1){if(p!==-1){var N=nn(e.substring(u,p).trim());if(!c.includes(N)){x!==";"&&m++;var C=e.substring(u,m).trim();r+=" "+C+";"}}u=m+1,p=-1}}}}return a&&(r+=os(a)),s&&(r+=os(s,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function Be(e,t,r,a,s,o){var i=e.__className;if(i!==r||i===void 0){var d=ul(r,a,o);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(o&&s!==o)for(var c in o){var u=!!o[c];(s==null||u!==!!s[c])&&e.classList.toggle(c,u)}return o}function sn(e,t={},r,a){for(var s in r){var o=r[s];t[s]!==o&&(r[s]==null?e.style.removeProperty(s):e.style.setProperty(s,o,a))}}function vl(e,t,r,a){var s=e.__style;if(s!==t){var o=fl(t,a);o==null?e.removeAttribute("style"):e.style.cssText=o,e.__style=t}else a&&(Array.isArray(a)?(sn(e,r==null?void 0:r[0],a[0]),sn(e,r==null?void 0:r[1],a[1],"important")):sn(e,r,a));return a}function $a(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Tn(t))return li();for(var a of e.options)a.selected=t.includes(Aa(a));return}for(a of e.options){var s=Aa(a);if(Ni(s,t)){a.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function Hn(e){var t=new MutationObserver(()=>{$a(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),Ya(()=>{t.disconnect()})}function $n(e,t,r=t){var a=new WeakSet,s=!0;Ws(e,"change",o=>{var i=o?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(i),Aa);else{var c=e.querySelector(i)??e.querySelector("option:not([disabled])");d=c&&Aa(c)}r(d),ge!==null&&a.add(ge)}),Xa(()=>{var o=t();if(e===document.activeElement){var i=za??ge;if(a.has(i))return}if($a(e,o,s),s&&o===void 0){var d=e.querySelector(":checked");d!==null&&(o=Aa(d),r(o))}e.__value=o,s=!1}),Hn(e)}function Aa(e){return"__value"in e?e.__value:e.value}const _a=Symbol("class"),xa=Symbol("style"),vo=Symbol("is custom element"),go=Symbol("is html"),gl=On?"option":"OPTION",pl=On?"select":"SELECT",bl=On?"progress":"PROGRESS";function Ra(e,t){var r=Bn(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==bl)||(e.value=t??"")}function hl(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function tt(e,t,r,a){var s=Bn(e);s[t]!==(s[t]=r)&&(t==="loading"&&(e[Do]=r),r==null?e.removeAttribute(t):typeof r!="string"&&po(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function yl(e,t,r,a,s=!1,o=!1){var i=Bn(e),d=i[vo],c=!i[go],u=t||{},p=e.nodeName===gl;for(var m in t)m in r||(r[m]=null);r.class?r.class=fo(r.class):r[_a]&&(r.class=null),r[xa]&&(r.style??(r.style=null));var x=po(e);for(const w in r){let v=r[w];if(p&&w==="value"&&v==null){e.value=e.__value="",u[w]=v;continue}if(w==="class"){var N=e.namespaceURI==="http://www.w3.org/1999/xhtml";Be(e,N,v,a,t==null?void 0:t[_a],r[_a]),u[w]=v,u[_a]=r[_a];continue}if(w==="style"){vl(e,v,t==null?void 0:t[xa],r[xa]),u[w]=v,u[xa]=r[xa];continue}var C=u[w];if(!(v===C&&!(v===void 0&&e.hasAttribute(w)))){u[w]=v;var F=w[0]+w[1];if(F!=="$$")if(F==="on"){const E={},S="$$"+w;let j=w.slice(2);var $=qi(j);if(Vi(j)&&(j=j.slice(0,-7),E.capture=!0),!$&&C){if(v!=null)continue;e.removeEventListener(j,u[S],E),u[S]=null}if($)ae(j,e,v),Xt([j]);else if(v!=null){let re=function(ce){u[w].call(this,ce)};var V=re;u[S]=lo(j,e,re,E)}}else if(w==="style")tt(e,w,v);else if(w==="autofocus")Li(e,!!v);else if(!d&&(w==="__value"||w==="value"&&v!=null))e.value=e.__value=v;else if(w==="selected"&&p)hl(e,v);else{var P=w;c||(P=Gi(P));var X=P==="defaultValue"||P==="defaultChecked";if(v==null&&!d&&!X)if(i[w]=null,P==="value"||P==="checked"){let E=e;const S=t===void 0;if(P==="value"){let j=E.defaultValue;E.removeAttribute(P),E.defaultValue=j,E.value=E.__value=S?j:null}else{let j=E.defaultChecked;E.removeAttribute(P),E.defaultChecked=j,E.checked=S?j:!1}}else e.removeAttribute(w);else X||x.includes(P)&&(d||typeof v!="string")?(e[P]=v,P in i&&(i[P]=vt)):typeof v!="function"&&tt(e,P,v)}}}return u}function is(e,t,r=[],a=[],s=[],o,i=!1,d=!1){Ls(s,r,a,c=>{var u=void 0,p={},m=e.nodeName===pl,x=!1;if(Ks(()=>{var C=t(...c.map(n)),F=yl(e,u,C,o,i,d);x&&m&&"value"in C&&$a(e,C.value);for(let P of Object.getOwnPropertySymbols(p))C[P]||mt(p[P]);for(let P of Object.getOwnPropertySymbols(C)){var $=C[P];P.description===ii&&(!u||$!==u[P])&&(p[P]&&mt(p[P]),p[P]=Mt(()=>dl(e,()=>$))),F[P]=$}u=F}),m){var N=e;Xa(()=>{$a(N,u.value,!0),Hn(N)})}x=!0})}function Bn(e){return e.__attributes??(e.__attributes={[vo]:e.nodeName.includes("-"),[go]:e.namespaceURI===ks})}var ls=new Map;function po(e){var t=e.getAttribute("is")||e.nodeName,r=ls.get(t);if(r)return r;ls.set(t,r=[]);for(var a,s=e,o=Element.prototype;o!==s;){a=No(s);for(var i in a)a[i].set&&r.push(i);s=hs(s)}return r}function Lr(e,t,r=t){var a=new WeakSet;Ws(e,"input",async s=>{var o=s?e.defaultValue:e.value;if(o=on(e)?ln(o):o,r(o),ge!==null&&a.add(ge),await no(),o!==(o=t())){var i=e.selectionStart,d=e.selectionEnd,c=e.value.length;if(e.value=o??"",d!==null){var u=e.value.length;i===d&&d===c&&u>c?(e.selectionStart=u,e.selectionEnd=u):(e.selectionStart=i,e.selectionEnd=Math.min(d,u))}}}),ga(t)==null&&e.value&&(r(on(e)?ln(e.value):e.value),ge!==null&&a.add(ge)),Dn(()=>{var s=t();if(e===document.activeElement){var o=za??ge;if(a.has(o))return}on(e)&&s===ln(e.value)||e.type==="date"&&!s&&!e.value||s!==e.value&&(e.value=s??"")})}function on(e){var t=e.type;return t==="number"||t==="range"}function ln(e){return e===""?null:+e}function ds(e,t){return e===t||(e==null?void 0:e[Ar])===t}function Mn(e={},t,r,a){return Xa(()=>{var s,o;return Dn(()=>{s=o,o=[],ga(()=>{e!==r(...o)&&(t(e,...o),s&&ds(r(...s),e)&&t(null,...s))})}),()=>{sr(()=>{o&&ds(r(...o),e)&&t(null,...o)})}}),e}let Fa=!1;function ml(e){var t=Fa;try{return Fa=!1,[e(),Fa]}finally{Fa=t}}const _l={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function Ye(e,t,r){return new Proxy({props:e,exclude:t},_l)}const xl={get(e,t){let r=e.props.length;for(;r--;){let a=e.props[r];if(ya(a)&&(a=a()),typeof a=="object"&&a!==null&&t in a)return a[t]}},set(e,t,r){let a=e.props.length;for(;a--;){let s=e.props[a];ya(s)&&(s=s());const o=wr(s,t);if(o&&o.set)return o.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let a=e.props[r];if(ya(a)&&(a=a()),typeof a=="object"&&a!==null&&t in a){const s=wr(a,t);return s&&!s.configurable&&(s.configurable=!0),s}}},has(e,t){if(t===Ar||t===ms)return!1;for(let r of e.props)if(ya(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(ya(r)&&(r=r()),!!r){for(const a in r)t.includes(a)||t.push(a);for(const a of Object.getOwnPropertySymbols(r))t.includes(a)||t.push(a)}return t}};function Xe(...e){return new Proxy({props:e},xl)}function Kr(e,t,r,a){var X;var s=(r&ri)!==0,o=(r&ai)!==0,i=a,d=!0,c=()=>(d&&(d=!1,i=o?ga(a):a),i),u;if(s){var p=Ar in e||ms in e;u=((X=wr(e,t))==null?void 0:X.set)??(p&&t in e?V=>e[t]=V:void 0)}var m,x=!1;s?[m,x]=ml(()=>e[t]):m=e[t],m===void 0&&a!==void 0&&(m=c(),u&&(Wo(),u(m)));var N;if(N=()=>{var V=e[t];return V===void 0?c():(d=!0,V)},!(r&ti))return N;if(u){var C=e.$$legacy;return function(V,w){return arguments.length>0?((!w||C||x)&&u(w?N():V),V):N()}}var F=!1,$=(r&ei?Ga:Rs)(()=>(F=!1,N()));s&&n($);var P=Te;return function(V,w){if(arguments.length>0){const v=w?n($):s?ot(V):V;return f($,v),F=!0,i!==void 0&&(i=v),V}return Mr&&F||P.f&nr?$.v:n($)}}function kl(e){Ct===null&&_s(),Pt(()=>{const t=ga(e);if(typeof t=="function")return t})}function wl(e){Ct===null&&_s(),kl(()=>()=>ga(e))}const Sl="5";var bs;typeof window<"u"&&((bs=window.__svelte??(window.__svelte={})).v??(bs.v=new Set)).add(Sl);const Vn="prx-console-token",Al=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}];function Ma(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(Vn))==null?void 0:e.trim())??""}function El(e){typeof window>"u"||window.localStorage.setItem(Vn,e.trim())}function bo(){typeof window>"u"||window.localStorage.removeItem(Vn)}const $l={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",notFound:"Not found",backToOverview:"Back to Overview"},Ml={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Cl={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},Pl={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},Tl={title:"Sessions",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No active sessions"},Nl={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Ol={title:"Channels",type:"Type",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI"}},Il={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings"},Ll={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},Rl={title:"Hooks",loading:"Loading hooks...",noHooks:"No hooks configured.",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",edit:"Edit",delete:"Delete",save:"Save",cancel:"Cancel"},Fl={title:"MCP Servers",loading:"Loading MCP servers...",noServers:"No MCP servers configured.",connected:"Connected",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},Dl={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},jl={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},Ul={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},zl={app:$l,nav:Ml,common:Cl,overview:Pl,sessions:Tl,chat:Nl,channels:Ol,config:Il,logs:Ll,hooks:Rl,mcp:Fl,skills:Dl,plugins:jl,login:Ul},Hl={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",notFound:"页面未找到",backToOverview:"返回概览"},Bl={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},Vl={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},Wl={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},ql={title:"会话",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"当前没有活跃会话"},Kl={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},Gl={title:"通道",type:"类型",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行"}},Jl={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项"},Yl={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},Xl={title:"Hooks",loading:"正在加载 Hooks...",noHooks:"尚未配置任何 Hook。",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",edit:"编辑",delete:"删除",save:"保存",cancel:"取消"},Ql={title:"MCP 服务",loading:"正在加载 MCP 服务...",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},Zl={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},ed={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},td={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},rd={app:Hl,nav:Bl,common:Vl,overview:Wl,sessions:ql,chat:Kl,channels:Gl,config:Jl,logs:Yl,hooks:Xl,mcp:Ql,skills:Zl,plugins:ed,login:td},Za="prx-console-lang",Ca="en",dn={en:zl,zh:rd};function Cn(e){return typeof e!="string"||e.trim().length===0?Ca:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function ad(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(Za);if(t)return Cn(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||Ca;return Cn(t)}return Ca}function cs(e,t){return t.split(".").reduce((r,a)=>{if(!(!r||typeof r!="object"))return r[a]},e)}function ho(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function nd(e){typeof window<"u"&&window.localStorage.setItem(Za,e)}const Vr=ot({lang:ad()});ho(Vr.lang);function yo(e){const t=Cn(e);Vr.lang!==t&&(Vr.lang=t,nd(t),ho(t))}function Gr(){yo(Vr.lang==="en"?"zh":"en")}function sd(){if(typeof window>"u")return;const e=window.localStorage.getItem(Za);e&&yo(e)}function y(e,t={}){const r=dn[Vr.lang]??dn[Ca];let a=cs(r,e);if(typeof a!="string"&&(a=cs(dn[Ca],e)),typeof a!="string")return e;for(const[s,o]of Object.entries(t))a=a.replaceAll(`{${s}}`,String(o));return a}function mo(){return typeof window>"u"?"/":window.location.pathname||"/"}function yr(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function od(e){if(typeof window>"u")return()=>{};const t=()=>{e(mo())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}/**
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
 */const id={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
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
 */const ld=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var dd=Zi("<svg><!><!></svg>");function Qe(e,t){_e(t,!0);const r=Kr(t,"color",3,"currentColor"),a=Kr(t,"size",3,24),s=Kr(t,"strokeWidth",3,2),o=Kr(t,"absoluteStrokeWidth",3,!1),i=Kr(t,"iconNode",19,()=>[]),d=Ye(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var c=dd();is(c,(m,x)=>({...id,...m,...d,width:a(),height:a(),stroke:r(),"stroke-width":x,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!ld(d)&&{"aria-hidden":"true"},()=>o()?Number(s())*24/Number(a()):s()]);var u=l(c);Ke(u,17,i,rt,(m,x)=>{var N=Ve(()=>vn(n(x),2));let C=()=>n(N)[0],F=()=>n(N)[1];var $=De(),P=Ne($);ll(P,C,!0,(X,V)=>{is(X,()=>({...F()}))}),h(m,$)});var p=b(u);Je(p,()=>t.children??Ge),h(e,c),xe()}function cd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];Qe(e,Xe({name:"badge-check"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function us(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];Qe(e,Xe({name:"blocks"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function ud(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];Qe(e,Xe({name:"bot"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function fd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];Qe(e,Xe({name:"brain"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function vd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];Qe(e,Xe({name:"cable"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function gd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];Qe(e,Xe({name:"chart-column"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function pd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];Qe(e,Xe({name:"circle-alert"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function bd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];Qe(e,Xe({name:"circle-check-big"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function hd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];Qe(e,Xe({name:"clock"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function yd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];Qe(e,Xe({name:"dollar-sign"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function md(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];Qe(e,Xe({name:"git-branch"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function _d(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];Qe(e,Xe({name:"globe"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function xd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];Qe(e,Xe({name:"heart-pulse"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function kd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];Qe(e,Xe({name:"loader"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function wd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];Qe(e,Xe({name:"message-square"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Sd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];Qe(e,Xe({name:"moon"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Ad(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];Qe(e,Xe({name:"paperclip"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function _o(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];Qe(e,Xe({name:"refresh-cw"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Ed(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];Qe(e,Xe({name:"search"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function $d(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];Qe(e,Xe({name:"settings"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Md(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];Qe(e,Xe({name:"shield"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Cd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];Qe(e,Xe({name:"sun"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}function Pd(e,t){_e(t,!0);/**
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
 */let r=Ye(t,["$$slots","$$events","$$legacy"]);const a=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];Qe(e,Xe({name:"zap"},()=>r,{get iconNode(){return a},children:(s,o)=>{var i=De(),d=Ne(i);Je(d,()=>t.children??Ge),h(s,i)},$$slots:{default:!0}})),xe()}var Td=A('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Nd=A('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function Od(e,t){_e(t,!0);let r=O(""),a=O("");function s(E){var j;E.preventDefault();const S=n(r).trim();if(!S){f(a,y("login.tokenRequired"),!0);return}El(S),f(a,""),(j=t.onLogin)==null||j.call(t,S)}var o=Nd(),i=l(o),d=l(i),c=l(d),u=l(c),p=b(c,2),m=l(p),x=b(d,2),N=l(x),C=b(x,2),F=l(C),$=l(F),P=b(F,2),X=b(P,2);{var V=E=>{var S=Td(),j=l(S);M(()=>g(j,n(a))),h(E,S)};ne(X,E=>{n(a)&&E(V)})}var w=b(X,2),v=l(w);M((E,S,j,re,ce,Ee)=>{g(u,E),tt(p,"aria-label",S),g(m,Vr.lang==="zh"?"中文 / EN":"EN / 中文"),g(N,j),g($,re),tt(P,"placeholder",ce),g(v,Ee)},[()=>y("login.title"),()=>y("app.language"),()=>y("login.hint"),()=>y("login.accessToken"),()=>y("login.placeholder"),()=>y("login.login")]),ae("click",p,function(...E){Gr==null||Gr.apply(this,E)}),qr("submit",C,s),Lr(P,()=>n(r),E=>f(r,E)),h(e,o),xe()}Xt(["click"]);const cn="".trim(),Ha=cn.endsWith("/")?cn.slice(0,-1):cn;class fs extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function Id(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Ld(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function pt(e,t={}){const r=Ma(),a={Accept:"application/json",...t.headers};r&&(a.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!a["Content-Type"]&&(a["Content-Type"]="application/json");const s=await fetch(`${Ha}${e}`,{...t,headers:a}),o=await Id(s);if(s.status===401)throw bo(),yr("/",!0),new fs(401,"Unauthorized");if(!s.ok)throw new fs(s.status,Ld(o,s.status));return o}const dt={getStatus:()=>pt("/api/status"),getSessions:()=>pt("/api/sessions"),getSessionMessages:e=>pt(`/api/sessions/${encodeURIComponent(e)}/messages`),sendMessage:(e,t)=>pt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return dt.sendMessage(e,t);const a=new FormData;a.append("message",t);for(const s of r)a.append("files",s);return pt(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:a})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e}),r=Ma();return r&&t.set("token",r),`${Ha}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>pt("/api/channels/status"),getConfig:()=>pt("/api/config"),saveConfig:e=>pt("/api/config",{method:"POST",body:JSON.stringify(e)}),getHooks:()=>pt("/api/hooks"),getMcpServers:()=>pt("/api/mcp/servers"),getSkills:()=>pt("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),pt(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>pt("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>pt(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>pt(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>pt("/api/plugins"),reloadPlugin:e=>pt(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})};function Rd(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),a=Math.floor(e%3600/60),s=Math.floor(e%60),o=[];return t>0&&o.push(`${t}d`),(r>0||o.length>0)&&o.push(`${r}h`),(a>0||o.length>0)&&o.push(`${a}m`),o.push(`${s}s`),o.join(" ")}var Fd=A('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Dd=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),jd=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Ud=A('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),zd=A('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Hd=A('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),Bd=A('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),Vd=A('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),Wd=A('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function qd(e,t){_e(t,!0);let r=O(null),a=O(!0),s=O(""),o=O("");function i(v){return typeof v!="string"||v.length===0?y("common.unknown"):v.replaceAll("_"," ").split(" ").map(E=>E.charAt(0).toUpperCase()+E.slice(1)).join(" ")}function d(v){const E=`channels.names.${v}`,S=y(E);return S===E?i(v):S}const c=Ve(()=>{var v,E,S,j,re;return[{label:y("overview.version"),value:((v=n(r))==null?void 0:v.version)??y("common.na")},{label:y("overview.uptime"),value:typeof((E=n(r))==null?void 0:E.uptime_seconds)=="number"?Rd(n(r).uptime_seconds):y("common.na")},{label:y("overview.model"),value:((S=n(r))==null?void 0:S.model)??y("common.na")},{label:y("overview.memoryBackend"),value:((j=n(r))==null?void 0:j.memory_backend)??y("common.na")},{label:y("overview.gatewayPort"),value:(re=n(r))!=null&&re.gateway_port?String(n(r).gateway_port):y("common.na")}]}),u=Ve(()=>{var v;return Array.isArray((v=n(r))==null?void 0:v.channels)?n(r).channels:[]});async function p(){try{const v=await dt.getStatus();f(r,v,!0),f(s,""),f(o,new Date().toLocaleTimeString(),!0)}catch(v){f(s,v instanceof Error?v.message:y("overview.loadFailed"),!0)}finally{f(a,!1)}}Pt(()=>{let v=!1;const E=async()=>{v||await p()};E();const S=setInterval(E,3e4);return()=>{v=!0,clearInterval(S)}});var m=Wd(),x=l(m),N=l(x),C=l(N),F=b(N,2);{var $=v=>{var E=Fd(),S=l(E);M(j=>g(S,j),[()=>y("common.updatedAt",{time:n(o)})]),h(v,E)};ne(F,v=>{n(o)&&v($)})}var P=b(x,2);{var X=v=>{var E=Dd(),S=l(E);M(j=>g(S,j),[()=>y("overview.loading")]),h(v,E)},V=v=>{var E=jd(),S=l(E);M(()=>g(S,n(s))),h(v,E)},w=v=>{var E=Vd(),S=Ne(E);Ke(S,21,()=>n(c),rt,(K,pe)=>{var se=Ud(),Ae=l(se),U=l(Ae),H=b(Ae,2),fe=l(H);M(()=>{g(U,n(pe).label),g(fe,n(pe).value)}),h(K,se)});var j=b(S,2),re=l(j),ce=l(re),Ee=b(re,2);{var $e=K=>{var pe=zd(),se=l(pe);M(Ae=>g(se,Ae),[()=>y("overview.noChannelsConfigured")]),h(K,pe)},B=K=>{var pe=Bd();Ke(pe,21,()=>n(u),rt,(se,Ae)=>{var U=Hd(),H=l(U);M(fe=>g(H,fe),[()=>d(n(Ae))]),h(se,U)}),h(K,pe)};ne(Ee,K=>{n(u).length===0?K($e):K(B,-1)})}M(K=>g(ce,K),[()=>y("overview.configuredChannels")]),h(v,E)};ne(P,v=>{n(a)?v(X):n(s)?v(V,1):v(w,-1)})}M(v=>g(C,v),[()=>y("overview.title")]),h(e,m),xe()}var Kd=A('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Gd=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Jd=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Yd=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Xd=A('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),Qd=A('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div>'),Zd=A('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function ec(e,t){_e(t,!0);let r=O(ot([])),a=O(!0),s=O(""),o=O("");function i(v){return typeof v!="string"||v.length===0?y("common.unknown"):v.replaceAll("_"," ").split(" ").map(E=>E.charAt(0).toUpperCase()+E.slice(1)).join(" ")}function d(v){const E=`channels.names.${v}`,S=y(E);return S===E?i(v):S}async function c(){try{const v=await dt.getSessions();f(r,Array.isArray(v)?v:[],!0),f(s,""),f(o,new Date().toLocaleTimeString(),!0)}catch(v){f(s,v instanceof Error?v.message:y("sessions.loadFailed"),!0)}finally{f(a,!1)}}function u(v){yr(`/chat/${encodeURIComponent(v)}`)}Pt(()=>{let v=!1;const E=async()=>{v||await c()};E();const S=setInterval(E,15e3);return()=>{v=!0,clearInterval(S)}});var p=Zd(),m=l(p),x=l(m),N=l(x),C=b(x,2);{var F=v=>{var E=Kd(),S=l(E);M(j=>g(S,j),[()=>y("common.updatedAt",{time:n(o)})]),h(v,E)};ne(C,v=>{n(o)&&v(F)})}var $=b(m,2);{var P=v=>{var E=Gd(),S=l(E);M(j=>g(S,j),[()=>y("sessions.loading")]),h(v,E)},X=v=>{var E=Jd(),S=l(E);M(()=>g(S,n(s))),h(v,E)},V=v=>{var E=Yd(),S=l(E);M(j=>g(S,j),[()=>y("sessions.none")]),h(v,E)},w=v=>{var E=Qd(),S=l(E),j=l(S),re=l(j),ce=l(re),Ee=l(ce),$e=b(ce),B=l($e),K=b($e),pe=l(K),se=b(K),Ae=l(se),U=b(se),H=l(U),fe=b(j);Ke(fe,21,()=>n(r),rt,(ye,oe)=>{var Y=Xd(),ie=l(Y),Me=l(ie),Le=b(ie),je=l(Le),Oe=b(Le),We=l(Oe),R=b(Oe),G=l(R),ue=b(R),Ue=l(ue);M((Ie,ze)=>{g(Me,n(oe).session_id),g(je,n(oe).sender),g(We,Ie),g(G,n(oe).message_count),g(Ue,ze)},[()=>d(n(oe).channel),()=>n(oe).last_message_preview||y("common.empty")]),ae("click",Y,()=>u(n(oe).session_id)),h(ye,Y)}),M((ye,oe,Y,ie,Me)=>{g(Ee,ye),g(B,oe),g(pe,Y),g(Ae,ie),g(H,Me)},[()=>y("sessions.sessionId"),()=>y("sessions.sender"),()=>y("sessions.channel"),()=>y("sessions.messages"),()=>y("sessions.lastMessage")]),h(v,E)};ne($,v=>{n(a)?v(P):n(s)?v(X,1):n(r).length===0?v(V,2):v(w,-1)})}M(v=>g(N,v),[()=>y("sessions.title")]),h(e,p),xe()}Xt(["click"]);var tc=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),rc=A('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),ac=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),nc=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),sc=A('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),oc=A('<img alt="Attachment" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),ic=A('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),lc=A("<div></div>"),dc=A('<div class="space-y-3"></div>'),cc=A('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),uc=A('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),fc=A('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),vc=A('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white">Remove</button></div>'),gc=A('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),pc=A('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region" aria-label="Chat messages"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" title="Attach files" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function bc(e,t){_e(t,!0);const r=10,a=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let s=Kr(t,"sessionId",3,""),o=O(ot([])),i=O(""),d=O(!0),c=O(!1),u=O(""),p=O(null),m=O(null),x=O(ot([])),N=O(!1),C=0;function F(){yr("/sessions")}function $(_){return _==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":_==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function P(_){return((_==null?void 0:_.type)||"").startsWith("image/")}function X(_){return((_==null?void 0:_.type)||"").startsWith("video/")}function V(_){if(!Number.isFinite(_)||_<=0)return"0 B";const z=["B","KB","MB","GB"];let D=_,te=0;for(;D>=1024&&te<z.length-1;)D/=1024,te+=1;return`${D.toFixed(te===0?0:1)} ${z[te]}`}function w(_){return typeof _=="string"&&_.trim().length>0?_:"unknown"}function v(_){const z=P(_),D=X(_);return{id:`${_.name}-${_.lastModified}-${Math.random().toString(36).slice(2)}`,file:_,name:_.name,size:_.size,type:w(_.type),isImage:z,isVideo:D,previewUrl:z||D?URL.createObjectURL(_):""}}function E(_){_&&typeof _.previewUrl=="string"&&_.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(_.previewUrl)}function S(){for(const _ of n(x))E(_);f(x,[],!0),n(m)&&(n(m).value="")}function j(_){if(!_||_.length===0||n(c))return;const z=Array.from(_),D=[],te=Math.max(0,r-n(x).length);for(const Fe of z.slice(0,te))D.push(v(Fe));f(x,[...n(x),...D],!0)}function re(_){const z=n(x).find(D=>D.id===_);z&&E(z),f(x,n(x).filter(D=>D.id!==_),!0)}function ce(){var _;n(c)||(_=n(m))==null||_.click()}function Ee(_){var z;j((z=_.currentTarget)==null?void 0:z.files),n(m)&&(n(m).value="")}function $e(_){_.preventDefault(),!n(c)&&(C+=1,f(N,!0))}function B(_){_.preventDefault(),!n(c)&&_.dataTransfer&&(_.dataTransfer.dropEffect="copy")}function K(_){_.preventDefault(),C=Math.max(0,C-1),C===0&&f(N,!1)}function pe(_){var z;_.preventDefault(),C=0,f(N,!1),j((z=_.dataTransfer)==null?void 0:z.files)}function se(_){const z=(_||"").trim();if(!z)return"";const D=z.toLowerCase();return D.startsWith("data:image/")||D.startsWith("data:video/")||D.startsWith("http://")||D.startsWith("https://")?z:dt.getSessionMediaUrl(z)}function Ae(_,z){const D=(z||"").trim().toLowerCase();return _==="VIDEO"||D.startsWith("data:video/")?"video":D.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(Fe=>D.endsWith(Fe))?"video":"image"}function U(_){if(typeof _!="string"||_.length===0)return[];const z=[];a.lastIndex=0;let D=0,te;for(;(te=a.exec(_))!==null;){te.index>D&&z.push({id:`text-${D}`,kind:"text",value:_.slice(D,te.index)});const Fe=(te[1]||"").toUpperCase(),le=(te[2]||te[3]||"").trim();if(le){const J=Ae(Fe,le);z.push({id:`${J}-${te.index}`,kind:J,value:le})}D=a.lastIndex}return D<_.length&&z.push({id:`text-tail-${D}`,kind:"text",value:_.slice(D)}),z}async function H(){await no(),n(p)&&(n(p).scrollTop=n(p).scrollHeight)}async function fe(){try{const _=await dt.getSessionMessages(s());f(o,Array.isArray(_)?_:[],!0),f(u,""),await H()}catch(_){f(u,_ instanceof Error?_.message:y("chat.loadFailed"),!0)}finally{f(d,!1)}}async function ye(){const _=n(i).trim(),z=n(x).map(te=>te.file);if(_.length===0&&z.length===0||n(c))return;f(c,!0),f(i,""),f(u,"");const D=z.length>0;D||(f(o,[...n(o),{role:"user",content:_}],!0),await H());try{const te=D?await dt.sendMessageWithMedia(s(),_,z):await dt.sendMessage(s(),_);D?await fe():te&&typeof te.reply=="string"&&te.reply.length>0&&f(o,[...n(o),{role:"assistant",content:te.reply}],!0),S()}catch(te){f(u,te instanceof Error?te.message:y("chat.sendFailed"),!0),await fe()}finally{f(c,!1),await H()}}function oe(_){_.preventDefault(),ye()}Pt(()=>{let _=!1;return(async()=>{_||(f(d,!0),await fe())})(),()=>{_=!0}}),wl(()=>{for(const _ of n(x))E(_)});var Y=pc(),ie=l(Y),Me=l(ie),Le=l(Me),je=l(Le),Oe=b(Le,2),We=l(Oe),R=b(Me,2),G=l(R),ue=b(ie,2);{var Ue=_=>{var z=tc(),D=l(z);M(()=>g(D,n(u))),h(_,z)};ne(ue,_=>{n(u)&&_(Ue)})}var Ie=b(ue,2),ze=l(Ie),T=l(ze);{var L=_=>{var z=rc(),D=l(z);M(()=>g(D,`Drop files to attach (${n(x).length??""}/10 selected)`)),h(_,z)};ne(T,_=>{n(N)&&_(L)})}var q=b(T,2);{var Q=_=>{var z=ac(),D=l(z);M(te=>g(D,te),[()=>y("chat.loading")]),h(_,z)},Z=_=>{var z=nc(),D=l(z);M(te=>g(D,te),[()=>y("chat.empty")]),h(_,z)},ke=_=>{var z=dc();Ke(z,21,()=>n(o),rt,(D,te)=>{var Fe=lc();Ke(Fe,21,()=>U(n(te).content),le=>le.id,(le,J)=>{var He=De(),gt=Ne(He);{var xt=at=>{var nt=De(),St=Ne(nt);{var lr=cr=>{var ft=sc(),kt=l(ft);M(()=>g(kt,n(J).value)),h(cr,ft)},dr=Ve(()=>n(J).value.trim().length>0);ne(St,cr=>{n(dr)&&cr(lr)})}h(at,nt)},Nt=at=>{var nt=oc();M(St=>tt(nt,"src",St),[()=>se(n(J).value)]),h(at,nt)},Ot=at=>{var nt=ic();M(St=>tt(nt,"src",St),[()=>se(n(J).value)]),h(at,nt)};ne(gt,at=>{n(J).kind==="text"?at(xt):n(J).kind==="image"?at(Nt,1):n(J).kind==="video"&&at(Ot,2)})}h(le,He)}),M(le=>Be(Fe,1,le),[()=>fo($(n(te).role))]),h(D,Fe)}),h(_,z)};ne(q,_=>{n(d)?_(Q):n(o).length===0?_(Z,1):_(ke,-1)})}Mn(ze,_=>f(p,_),()=>n(p));var ee=b(ze,2),be=l(ee);Mn(be,_=>f(m,_),()=>n(m));var de=b(be,2);{var ve=_=>{var z=gc(),D=l(z),te=l(D),Fe=b(D,2);Ke(Fe,21,()=>n(x),le=>le.id,(le,J)=>{var He=vc(),gt=l(He);{var xt=ft=>{var kt=cc();M(()=>{tt(kt,"src",n(J).previewUrl),tt(kt,"alt",n(J).name)}),h(ft,kt)},Nt=ft=>{var kt=uc();kt.muted=!0,M(()=>tt(kt,"src",n(J).previewUrl)),h(ft,kt)},Ot=ft=>{var kt=fc();h(ft,kt)};ne(gt,ft=>{n(J).isImage?ft(xt):n(J).isVideo?ft(Nt,1):ft(Ot,-1)})}var at=b(gt,2),nt=l(at),St=l(nt),lr=b(nt,2),dr=l(lr),cr=b(at,2);M(ft=>{g(St,n(J).name),g(dr,`${n(J).type??""} · ${ft??""}`)},[()=>V(n(J).size)]),ae("click",cr,()=>re(n(J).id)),h(le,He)}),M(()=>g(te,`Attachments (${n(x).length??""}/10)`)),h(_,z)};ne(de,_=>{n(x).length>0&&_(ve)})}var I=b(de,2),W=l(I),me=b(W,2),Ce=l(me);Ad(Ce,{size:16});var Re=b(me,2),qe=l(Re);M((_,z,D,te,Fe,le)=>{g(je,_),g(We,`${z??""}: ${s()??""}`),g(G,D),Be(ze,1,`flex-1 overflow-y-auto p-4 ${n(N)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),tt(W,"placeholder",te),me.disabled=n(c)||n(x).length>=r,Re.disabled=Fe,g(qe,le)},[()=>y("chat.title"),()=>y("chat.session"),()=>y("chat.back"),()=>y("chat.inputPlaceholder"),()=>n(c)||!n(i).trim()&&n(x).length===0,()=>n(c)?y("chat.sending"):y("chat.send")]),ae("click",R,F),qr("dragenter",Ie,$e),qr("dragover",Ie,B),qr("dragleave",Ie,K),qr("drop",Ie,pe),qr("submit",ee,oe),ae("change",be,Ee),Lr(W,()=>n(i),_=>f(i,_)),ae("click",me,ce),h(e,Y),xe()}Xt(["click","change"]);var hc=A('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),yc=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),mc=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),_c=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),xc=A('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),kc=A('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),wc=A('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Sc(e,t){_e(t,!0);let r=O(ot([])),a=O(!0),s=O(""),o=O("");function i(w){return typeof w!="string"||w.length===0?y("common.unknown"):w.replaceAll("_"," ").split(" ").map(v=>v.charAt(0).toUpperCase()+v.slice(1)).join(" ")}function d(w){const v=`channels.names.${w}`,E=y(v);return E===v?i(w):E}async function c(){try{const w=await dt.getChannelsStatus();f(r,Array.isArray(w==null?void 0:w.channels)?w.channels:[],!0),f(s,""),f(o,new Date().toLocaleTimeString(),!0)}catch(w){f(s,w instanceof Error?w.message:y("channels.loadFailed"),!0)}finally{f(a,!1)}}Pt(()=>{let w=!1;const v=async()=>{w||await c()};v();const E=setInterval(v,3e4);return()=>{w=!0,clearInterval(E)}});var u=wc(),p=l(u),m=l(p),x=l(m),N=b(m,2);{var C=w=>{var v=hc(),E=l(v);M(S=>g(E,S),[()=>y("common.updatedAt",{time:n(o)})]),h(w,v)};ne(N,w=>{n(o)&&w(C)})}var F=b(p,2);{var $=w=>{var v=yc(),E=l(v);M(S=>g(E,S),[()=>y("channels.loading")]),h(w,v)},P=w=>{var v=mc(),E=l(v);M(()=>g(E,n(s))),h(w,v)},X=w=>{var v=_c(),E=l(v);M(S=>g(E,S),[()=>y("channels.noChannels")]),h(w,v)},V=w=>{var v=kc();Ke(v,21,()=>n(r),rt,(E,S)=>{var j=xc(),re=l(j),ce=l(re),Ee=l(ce),$e=b(ce,2),B=l($e),K=b(re,2),pe=l(K);M((se,Ae,U,H)=>{g(Ee,se),Be($e,1,`rounded-full px-2 py-1 text-xs font-medium ${n(S).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),g(B,Ae),g(pe,`${U??""}: ${H??""}`)},[()=>d(n(S).name),()=>n(S).enabled?y("common.enabled"):y("common.disabled"),()=>y("channels.type"),()=>d(n(S).type)]),h(E,j)}),h(w,v)};ne(F,w=>{n(a)?w($):n(s)?w(P,1):n(r).length===0?w(X,2):w(V,-1)})}M(w=>g(x,w),[()=>y("channels.title")]),h(e,u),xe()}function un(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;")}const vs=/(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;function Ac(e){return e.startsWith('"')?e.endsWith(":")?"text-sky-300":"text-emerald-300":e==="true"||e==="false"?"text-amber-300":e==="null"?"text-fuchsia-300":"text-violet-300"}function Ec(e){if(!e)return"";let t="",r=0;vs.lastIndex=0;for(const a of e.matchAll(vs)){const s=a.index??0,o=a[0];t+=un(e.slice(r,s)),t+=`<span class="${Ac(o)}">${un(o)}</span>`,r=s+o.length}return t+=un(e.slice(r)),t}var $c=A('<p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>'),Mc=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Cc=A('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950"><pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code><!></code></pre></div>'),Pc=A('<span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),Tc=A('<span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>'),Nc=A('<button type="button"><span></span></button>'),Oc=A("<option> </option>"),Ic=A('<select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select>'),Lc=A('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Rc=A('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400">×</button></div>'),Fc=A('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400">+ 添加</button></div>'),Dc=A('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"> </button></div>'),jc=A('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Uc=A('<div><div class="flex items-start justify-between gap-3"><div class="flex-1 min-w-0"><label class="block text-sm font-medium text-gray-700 dark:text-gray-200"> <!></label> <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex-shrink-0 w-64"><!></div></div></div>'),zc=A('<details class="group rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100"><!> <span> </span> <!></summary> <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700"></div></details>'),Hc=A('<div class="space-y-3"></div>'),Bc=A('<div class="flex items-start gap-2 text-xs"><span class="flex-shrink-0 text-gray-400 dark:text-gray-500"> </span> <span class="font-medium text-gray-600 dark:text-gray-300"> </span> <span class="text-red-500 line-through dark:text-red-400"> </span> <span class="text-gray-400 dark:text-gray-600">→</span> <span class="text-green-600 dark:text-green-400"> </span></div>'),Vc=A('<div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950"><p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p> <div class="space-y-1.5 max-h-48 overflow-y-auto"></div></div>'),Wc=A('<div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95"><div class="mx-auto flex max-w-5xl items-center justify-between gap-4"><div class="flex items-center gap-3"><span class="text-sm text-sky-600 dark:text-sky-400"> </span> <button type="button" class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"> </button></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">放弃修改</button> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div> <!></div>'),qc=A("<div> </div>"),Kc=A('<section class="space-y-4 pb-24"><div class="flex items-center justify-between gap-4"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">复制 JSON</button></div></div> <!> <!> <!></section>');function Gc(e,t){_e(t,!0);let r=O(null),a=O(null),s=O(null),o=O(!0),i=O(!1),d=O(""),c=O(""),u=O(!1),p=O(!1),m=O(ot(new Set));const x={provider:Pd,gateway:_d,channels:wd,agent:ud,memory:fd,security:Md,heartbeat:xd,reliability:_o,scheduler:hd,sessions_spawn:md,observability:gd,web_search:Ed,cost:yd,runtime:$d,tunnel:vd,identity:cd},N={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","zai","compatible","copilot"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function C(T,L){if(!T)return;const q=L.split(".");let Q=T;for(const Z of q){if(Q==null||typeof Q!="object")return;Q=Q[Z]}return Q}function F(T,L,q){const Q=L.split(".");let Z=T;for(let ke=0;ke<Q.length-1;ke++)(Z[Q[ke]]==null||typeof Z[Q[ke]]!="object")&&(Z[Q[ke]]={}),Z=Z[Q[ke]];Z[Q[Q.length-1]]=q}function $(T){if(n(r))return C(n(r),T)}function P(T){return JSON.parse(JSON.stringify(T))}function X(T,L){return JSON.stringify(T)===JSON.stringify(L)}function V(){if(!n(r)||!n(a))return[];const T=[];for(const[L,q]of Object.entries(N))for(const[Q,Z]of Object.entries(q.fields)){const ke=C(n(a),Q),ee=C(n(r),Q);X(ke,ee)||T.push({group:q.label,fieldPath:Q,label:Z.label,oldVal:ke,newVal:ee})}return T}const w=Ve(()=>!n(r)||!n(a)?!1:V().length>0),v=Ve(V),E=Ve(()=>new Set(n(v).map(T=>T.fieldPath))),S=Ve(()=>n(r)?JSON.stringify(n(r),null,2):""),j=Ve(()=>Ec(n(S)));function re(T,L){if(!n(r))return;const q=P(n(r));F(q,T,L),f(r,q,!0)}function ce(T){const L=$(T),q=Array.isArray(L)?[...L,""]:[""];re(T,q)}function Ee(T,L){const q=$(T);if(!Array.isArray(q))return;const Q=q.filter((Z,ke)=>ke!==L);re(T,Q)}function $e(T,L,q){const Q=$(T);if(!Array.isArray(Q))return;const Z=[...Q];Z[L]=q,re(T,Z)}function B(T){const L=new Set(n(m));L.has(T)?L.delete(T):L.add(T),f(m,L,!0)}function K(T){return T==null?"null":typeof T=="boolean"?T?"true":"false":Array.isArray(T)||typeof T=="object"?JSON.stringify(T):String(T)}async function pe(){try{const[T,L]=await Promise.all([dt.getConfig(),dt.getStatus().catch(()=>null)]);f(r,typeof T=="object"&&T?T:{},!0),f(a,P(n(r)),!0),f(s,L,!0),f(d,"")}catch(T){f(d,T instanceof Error?T.message:"Failed to load config",!0)}finally{f(o,!1)}}async function se(){if(!(!n(w)||n(i))){f(i,!0),f(c,"");try{const T={};for(const q of n(v))F(T,q.fieldPath,q.newVal);const L=await dt.saveConfig(T);f(a,P(n(r)),!0),f(p,!1),L!=null&&L.restart_required?f(c,"已保存，部分设置需要重启服务后生效"):f(c,"已保存"),setTimeout(()=>{f(c,"")},5e3)}catch(T){f(c,"保存失败: "+(T instanceof Error?T.message:String(T)))}finally{f(i,!1)}}}function Ae(){n(a)&&(f(r,P(n(a)),!0),f(p,!1))}async function U(){if(!(!n(S)||typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(n(S))}catch{}}Pt(()=>{pe()});var H=Kc(),fe=l(H),ye=l(fe),oe=l(ye),Y=b(ye,2),ie=l(Y),Me=l(ie),Le=b(ie,2),je=b(fe,2);{var Oe=T=>{var L=$c();h(T,L)},We=T=>{var L=Mc(),q=l(L);M(()=>g(q,n(d))),h(T,L)},R=T=>{var L=Cc(),q=l(L),Q=l(q),Z=l(Q);ol(Z,()=>n(j)),h(T,L)},G=T=>{var L=Hc();Ke(L,21,()=>Object.entries(N),rt,(q,Q)=>{var Z=Ve(()=>vn(n(Q),2));let ke=()=>n(Z)[0],ee=()=>n(Z)[1];const be=Ve(()=>x[ke()]);var de=zc(),ve=l(de),I=l(ve);{var W=D=>{var te=De(),Fe=Ne(te);il(Fe,()=>n(be),(le,J)=>{J(le,{size:18,class:"text-gray-500 dark:text-gray-400"})}),h(D,te)};ne(I,D=>{n(be)&&D(W)})}var me=b(I,2),Ce=l(me),Re=b(me,2);{var qe=D=>{var te=Pc();h(D,te)},_=Ve(()=>[...Object.keys(ee().fields)].some(D=>n(E).has(D)));ne(Re,D=>{n(_)&&D(qe)})}var z=b(ve,2);Ke(z,21,()=>Object.entries(ee().fields),rt,(D,te)=>{var Fe=Ve(()=>vn(n(te),2));let le=()=>n(Fe)[0],J=()=>n(Fe)[1];const He=Ve(()=>$(le())),gt=Ve(()=>n(E).has(le())),xt=Ve(()=>n(m).has(le()));var Nt=Uc(),Ot=l(Nt),at=l(Ot),nt=l(at),St=l(nt),lr=b(St);{var dr=et=>{var Pe=Tc();h(et,Pe)};ne(lr,et=>{n(gt)&&et(dr)})}var cr=b(nt,2),ft=l(cr),kt=b(at,2),en=l(kt);{var Ze=et=>{var Pe=Nc(),lt=l(Pe);M(()=>{Be(Pe,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${n(He)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Be(lt,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${n(He)?"translate-x-6":"translate-x-1"}`)}),ae("click",Pe,()=>re(le(),!n(He))),h(et,Pe)},it=et=>{var Pe=Ic();Ke(Pe,21,()=>J().options,rt,(Bt,Pr)=>{var br=Oc(),ba=l(br),ha={};M(()=>{g(ba,n(Pr)||"(默认)"),ha!==(ha=n(Pr))&&(br.value=(br.__value=n(Pr))??"")}),h(Bt,br)});var lt;Hn(Pe),M(()=>{lt!==(lt=n(He)??J().default)&&(Pe.value=(Pe.__value=n(He)??J().default)??"",$a(Pe,n(He)??J().default))}),ae("change",Pe,Bt=>re(le(),Bt.target.value)),h(et,Pe)},It=et=>{var Pe=Lc();M(lt=>{Ra(Pe,n(He)??J().default),tt(Pe,"min",J().min),tt(Pe,"max",J().max),tt(Pe,"step",J().step??1),tt(Pe,"placeholder",lt)},[()=>String(J().default)]),ae("input",Pe,lt=>{const Bt=J().step&&J().step<1?parseFloat(lt.target.value):parseInt(lt.target.value,10);isNaN(Bt)||re(le(),Bt)}),h(et,Pe)},pa=et=>{var Pe=Fc(),lt=l(Pe);{var Bt=ba=>{var ha=De(),wo=Ne(ha);Ke(wo,17,()=>n(He),rt,(So,Ao,qn)=>{var Kn=Rc(),tn=l(Kn),Eo=b(tn,2);M(()=>Ra(tn,n(Ao))),ae("input",tn,$o=>$e(le(),qn,$o.target.value)),ae("click",Eo,()=>Ee(le(),qn)),h(So,Kn)}),h(ba,ha)},Pr=Ve(()=>Array.isArray(n(He)));ne(lt,ba=>{n(Pr)&&ba(Bt)})}var br=b(lt,2);ae("click",br,()=>ce(le())),h(et,Pe)},xo=et=>{var Pe=Dc(),lt=l(Pe),Bt=b(lt,2),Pr=l(Bt);M(()=>{tt(lt,"type",n(xt)?"text":"password"),Ra(lt,n(He)??""),tt(lt,"placeholder",J().default||"未设置"),g(Pr,n(xt)?"隐藏":"显示")}),ae("input",lt,br=>re(le(),br.target.value)),ae("click",Bt,()=>B(le())),h(et,Pe)},ko=et=>{var Pe=jc();M(()=>{Ra(Pe,n(He)??""),tt(Pe,"placeholder",J().default||"未设置")}),ae("input",Pe,lt=>re(le(),lt.target.value)),h(et,Pe)};ne(en,et=>{J().type==="bool"?et(Ze):J().type==="enum"?et(it,1):J().type==="number"?et(It,2):J().type==="array"?et(pa,3):J().sensitive?et(xo,4):et(ko,-1)})}M(()=>{Be(Nt,1,`rounded-lg border p-3 transition-colors ${n(gt)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`),g(St,`${J().label??""} `),g(ft,J().desc)}),h(D,Nt)}),M(()=>{de.open=ee().defaultOpen,g(Ce,ee().label)}),h(q,de)}),h(T,L)};ne(je,T=>{n(o)?T(Oe):n(d)?T(We,1):n(u)?T(R,2):T(G,-1)})}var ue=b(je,2);{var Ue=T=>{var L=Wc(),q=l(L),Q=l(q),Z=l(Q),ke=l(Z),ee=b(Z,2),be=l(ee),de=b(Q,2),ve=l(de),I=b(ve,2),W=l(I),me=b(q,2);{var Ce=Re=>{var qe=Vc(),_=b(l(qe),2);Ke(_,21,()=>n(v),rt,(z,D)=>{var te=Bc(),Fe=l(te),le=l(Fe),J=b(Fe,2),He=l(J),gt=b(J,2),xt=l(gt),Nt=b(gt,4),Ot=l(Nt);M((at,nt)=>{g(le,n(D).group),g(He,n(D).label),g(xt,at),g(Ot,nt)},[()=>K(n(D).oldVal),()=>K(n(D).newVal)]),h(z,te)}),h(Re,qe)};ne(me,Re=>{n(p)&&Re(Ce)})}M(()=>{g(ke,`${n(v).length??""} 项更改`),g(be,n(p)?"隐藏详情":"查看详情"),I.disabled=n(i),g(W,n(i)?"保存中...":"保存配置")}),ae("click",ee,()=>f(p,!n(p))),ae("click",ve,Ae),ae("click",I,se),h(T,L)};ne(ue,T=>{n(w)&&!n(o)&&!n(u)&&T(Ue)})}var Ie=b(ue,2);{var ze=T=>{var L=qc(),q=l(L);M(Q=>{Be(L,1,`fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg ${Q??""}`),g(q,n(c))},[()=>n(c).startsWith("保存失败")?"border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"]),h(T,L)};ne(Ie,T=>{n(c)&&T(ze)})}M(T=>{g(oe,T),g(Me,n(u)?"结构化编辑":"JSON 视图")},[()=>y("config.title")]),ae("click",ie,()=>f(u,!n(u))),ae("click",Le,U),h(e,H),xe()}Xt(["click","change","input"]);var Jc=A('<p class="text-gray-400 dark:text-gray-500"> </p>'),Yc=A('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Xc=A('<ol class="space-y-1"></ol>'),Qc=A('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Zc(e,t){_e(t,!0);const r=1e3,a=500,s=1e4;let o=O(ot([])),i=O(!1),d=O("disconnected"),c=O(null),u=null,p=null,m=0,x=!0;const N=Ve(()=>n(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":n(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),C=Ve(()=>n(d)==="connected"?y("logs.connected"):n(d)==="reconnecting"?y("logs.reconnecting"):y("logs.disconnected"));function F(oe){const Y=Ha?new URL(Ha,window.location.href):new URL(window.location.href);return Y.protocol=Y.protocol==="https:"?"wss:":"ws:",Y.pathname="/api/logs/stream",Y.search=`token=${encodeURIComponent(oe)}`,Y.hash="",Y.toString()}function $(oe){if(typeof oe!="string"||oe.length===0)return;const Y=oe.split(/\r?\n/).filter(Me=>Me.length>0);if(Y.length===0)return;const ie=[...n(o),...Y];f(o,ie.length>r?ie.slice(ie.length-r):ie,!0)}function P(){p!==null&&(clearTimeout(p),p=null)}function X(){u&&(u.onopen=null,u.onmessage=null,u.onerror=null,u.onclose=null,u.close(),u=null)}function V(){if(!x){f(d,"disconnected");return}f(d,"reconnecting");const oe=Math.min(a*2**m,s);m+=1,P(),p=setTimeout(()=>{p=null,w()},oe)}function w(){P();const oe=Ma();if(!oe){f(d,"disconnected");return}f(d,"reconnecting"),X();let Y;try{Y=new WebSocket(F(oe))}catch{V();return}u=Y,Y.onopen=()=>{m=0,f(d,"connected")},Y.onmessage=ie=>{n(i)||$(ie.data)},Y.onerror=()=>{(Y.readyState===WebSocket.OPEN||Y.readyState===WebSocket.CONNECTING)&&Y.close()},Y.onclose=()=>{u=null,V()}}function v(){f(i,!n(i))}function E(){f(o,[],!0)}Pt(()=>(x=!0,w(),()=>{x=!1,P(),X(),f(d,"disconnected")})),Pt(()=>{n(o).length,n(i),!(n(i)||!n(c))&&queueMicrotask(()=>{n(c)&&(n(c).scrollTop=n(c).scrollHeight)})});var S=Qc(),j=l(S),re=l(j),ce=l(re),Ee=b(re,2),$e=l(Ee),B=l($e),K=b($e,2),pe=l(K),se=b(K,2),Ae=l(se),U=b(j,2),H=l(U);{var fe=oe=>{var Y=Jc(),ie=l(Y);M(Me=>g(ie,Me),[()=>y("logs.waiting")]),h(oe,Y)},ye=oe=>{var Y=Xc();Ke(Y,21,()=>n(o),rt,(ie,Me,Le)=>{var je=Yc(),Oe=l(je),We=l(Oe),R=b(Oe,2),G=l(R);M(ue=>{g(We,ue),g(G,n(Me))},[()=>String(Le+1).padStart(4,"0")]),h(ie,je)}),h(oe,Y)};ne(H,oe=>{n(o).length===0?oe(fe):oe(ye,-1)})}Mn(U,oe=>f(c,oe),()=>n(c)),M((oe,Y,ie)=>{g(ce,oe),Be($e,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${n(N)}`),g(B,n(C)),g(pe,Y),g(Ae,ie)},[()=>y("logs.title"),()=>n(i)?y("logs.resume"):y("logs.pause"),()=>y("logs.clear")]),ae("click",K,v),ae("click",se,E),h(e,S),xe()}Xt(["click"]);var eu=A("<option> </option>"),tu=A('<div class="rounded-xl border border-sky-500/30 bg-white p-4 space-y-3 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),ru=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),au=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),nu=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),su=A("<option> </option>"),ou=A('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),iu=A('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button"><span></span></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 dark:text-red-300"> </button></div></div>'),lu=A('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),du=A('<div class="space-y-3"></div>'),cu=A('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <!> <!></section>');function uu(e,t){_e(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call_end","message_received","message_sent"];let a=O(ot([])),s=O(!0),o=O(""),i=O(null),d=O(!1),c=O(ot(r[0])),u=O(""),p=O(3e4),m=O(!0);function x(){f(c,r[0],!0),f(u,""),f(p,3e4),f(m,!0)}function N(U){return U.split("_").map(H=>H.charAt(0).toUpperCase()+H.slice(1)).join(" ")}async function C(){try{const U=await dt.getHooks();f(a,Array.isArray(U==null?void 0:U.hooks)?U.hooks:[],!0),f(o,"")}catch{f(a,[{id:"1",event:"message_received",command:'echo "msg received"',timeout_ms:3e4,enabled:!0},{id:"2",event:"agent_start",command:"/opt/scripts/on-start.sh",timeout_ms:1e4,enabled:!0},{id:"3",event:"tool_call_end",command:'notify-send "tool done"',timeout_ms:5e3,enabled:!1}],!0),f(o,"")}finally{f(s,!1)}}function F(U){f(i,U.id,!0),f(c,U.event,!0),f(u,U.command,!0),f(p,U.timeout_ms,!0),f(m,U.enabled,!0)}function $(){f(i,null),x()}function P(U){f(a,n(a).map(H=>H.id===U?{...H,event:n(c),command:n(u),timeout_ms:n(p),enabled:n(m)}:H),!0),f(i,null),x()}function X(){if(!n(u).trim())return;const U={id:String(Date.now()),event:n(c),command:n(u).trim(),timeout_ms:n(p),enabled:n(m)};f(a,[...n(a),U],!0),f(d,!1),x()}function V(U){f(a,n(a).filter(H=>H.id!==U),!0)}function w(U){f(a,n(a).map(H=>H.id===U?{...H,enabled:!H.enabled}:H),!0)}Pt(()=>{C()});var v=cu(),E=l(v),S=l(E),j=l(S),re=b(S,2),ce=l(re),Ee=b(E,2);{var $e=U=>{var H=tu(),fe=l(H),ye=l(fe),oe=b(fe,2),Y=l(oe),ie=l(Y),Me=l(ie),Le=b(ie,2);Ke(Le,21,()=>r,rt,(ve,I)=>{var W=eu(),me=l(W),Ce={};M(Re=>{g(me,Re),Ce!==(Ce=n(I))&&(W.value=(W.__value=n(I))??"")},[()=>N(n(I))]),h(ve,W)});var je=b(Y,2),Oe=l(je),We=l(Oe),R=b(Oe,2),G=b(je,2),ue=l(G),Ue=l(ue),Ie=b(ue,2),ze=b(G,2),T=l(ze),L=l(T),q=b(T,2),Q=l(q),Z=b(oe,2),ke=l(Z),ee=l(ke),be=b(ke,2),de=l(be);M((ve,I,W,me,Ce,Re,qe,_)=>{g(ye,ve),g(Me,I),g(We,W),g(Ue,me),tt(Ie,"placeholder",Ce),g(L,Re),Be(q,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${n(m)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Be(Q,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${n(m)?"translate-x-4":"translate-x-1"}`),g(ee,qe),g(de,_)},[()=>y("hooks.newHook"),()=>y("hooks.event"),()=>y("hooks.timeout"),()=>y("hooks.command"),()=>y("hooks.commandPlaceholder"),()=>y("hooks.enabled"),()=>y("hooks.cancel"),()=>y("hooks.save")]),$n(Le,()=>n(c),ve=>f(c,ve)),Lr(R,()=>n(p),ve=>f(p,ve)),Lr(Ie,()=>n(u),ve=>f(u,ve)),ae("click",q,()=>f(m,!n(m))),ae("click",ke,()=>{f(d,!1),x()}),ae("click",be,X),h(U,H)};ne(Ee,U=>{n(d)&&U($e)})}var B=b(Ee,2);{var K=U=>{var H=ru(),fe=l(H);M(ye=>g(fe,ye),[()=>y("hooks.loading")]),h(U,H)},pe=U=>{var H=au(),fe=l(H);M(()=>g(fe,n(o))),h(U,H)},se=U=>{var H=nu(),fe=l(H);M(ye=>g(fe,ye),[()=>y("hooks.noHooks")]),h(U,H)},Ae=U=>{var H=du();Ke(H,21,()=>n(a),fe=>fe.id,(fe,ye)=>{var oe=lu(),Y=l(oe);{var ie=Le=>{var je=ou(),Oe=l(je),We=l(Oe),R=l(We),G=l(R),ue=b(R,2);Ke(ue,21,()=>r,rt,(qe,_)=>{var z=su(),D=l(z),te={};M(Fe=>{g(D,Fe),te!==(te=n(_))&&(z.value=(z.__value=n(_))??"")},[()=>N(n(_))]),h(qe,z)});var Ue=b(We,2),Ie=l(Ue),ze=l(Ie),T=b(Ie,2),L=b(Ue,2),q=l(L),Q=l(q),Z=b(q,2),ke=b(L,2),ee=l(ke),be=l(ee),de=b(ee,2),ve=l(de),I=b(Oe,2),W=l(I),me=l(W),Ce=b(W,2),Re=l(Ce);M((qe,_,z,D,te,Fe)=>{g(G,qe),g(ze,_),g(Q,z),g(be,D),Be(de,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${n(m)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Be(ve,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${n(m)?"translate-x-4":"translate-x-1"}`),g(me,te),g(Re,Fe)},[()=>y("hooks.event"),()=>y("hooks.timeout"),()=>y("hooks.command"),()=>y("hooks.enabled"),()=>y("hooks.cancel"),()=>y("hooks.save")]),$n(ue,()=>n(c),qe=>f(c,qe)),Lr(T,()=>n(p),qe=>f(p,qe)),Lr(Z,()=>n(u),qe=>f(u,qe)),ae("click",de,()=>f(m,!n(m))),ae("click",W,$),ae("click",Ce,()=>P(n(ye).id)),h(Le,je)},Me=Le=>{var je=iu(),Oe=l(je),We=l(Oe),R=l(We),G=l(R),ue=b(R,2),Ue=l(ue),Ie=b(We,2),ze=l(Ie),T=b(Ie,2),L=l(T),q=b(Oe,2),Q=l(q),Z=l(Q),ke=b(Q,2),ee=l(ke),be=b(ke,2),de=l(be);M((ve,I,W,me,Ce)=>{g(G,ve),Be(ue,1,`rounded-full px-2 py-1 text-xs font-medium ${n(ye).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),g(Ue,I),g(ze,n(ye).command),g(L,`${W??""}: ${n(ye).timeout_ms??""}ms`),Be(Q,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${n(ye).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Be(Z,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${n(ye).enabled?"translate-x-4":"translate-x-1"}`),g(ee,me),g(de,Ce)},[()=>N(n(ye).event),()=>n(ye).enabled?y("common.enabled"):y("common.disabled"),()=>y("hooks.timeout"),()=>y("hooks.edit"),()=>y("hooks.delete")]),ae("click",Q,()=>w(n(ye).id)),ae("click",ke,()=>F(n(ye))),ae("click",be,()=>V(n(ye).id)),h(Le,je)};ne(Y,Le=>{n(i)===n(ye).id?Le(ie):Le(Me,-1)})}h(fe,oe)}),h(U,H)};ne(B,U=>{n(s)?U(K):n(o)?U(pe,1):n(a).length===0?U(se,2):U(Ae,-1)})}M((U,H)=>{g(j,U),g(ce,H)},[()=>y("hooks.title"),()=>n(d)?y("hooks.cancelAdd"):y("hooks.addHook")]),ae("click",re,()=>{f(d,!n(d)),n(d)&&x()}),h(e,v),xe()}Xt(["click"]);var fu=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),vu=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),gu=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),pu=A('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),bu=A('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),hu=A('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),yu=A('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),mu=A('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),_u=A('<div class="space-y-4"></div>'),xu=A('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function ku(e,t){_e(t,!0);let r=O(ot([])),a=O(!0),s=O(""),o=O(null);async function i(){try{const w=await dt.getMcpServers();f(r,Array.isArray(w==null?void 0:w.servers)?w.servers:[],!0),f(s,"")}catch{f(r,[{name:"filesystem",url:"stdio:///usr/local/bin/mcp-filesystem",status:"connected",tools:[{name:"read_file",description:"Read contents of a file"},{name:"write_file",description:"Write content to a file"},{name:"list_directory",description:"List directory contents"}]},{name:"github",url:"https://mcp.github.com/sse",status:"connected",tools:[{name:"search_repositories",description:"Search GitHub repositories"},{name:"create_issue",description:"Create a new issue"},{name:"list_pull_requests",description:"List pull requests"}]},{name:"database",url:"stdio:///opt/mcp/db-server",status:"disconnected",tools:[]}],!0),f(s,"")}finally{f(a,!1)}}function d(w){f(o,n(o)===w?null:w,!0)}async function c(){f(a,!0),await i()}Pt(()=>{i()});var u=xu(),p=l(u),m=l(p),x=l(m),N=b(m,2),C=l(N),F=b(p,2);{var $=w=>{var v=fu(),E=l(v);M(S=>g(E,S),[()=>y("mcp.loading")]),h(w,v)},P=w=>{var v=vu(),E=l(v);M(()=>g(E,n(s))),h(w,v)},X=w=>{var v=gu(),E=l(v);M(S=>g(E,S),[()=>y("mcp.noServers")]),h(w,v)},V=w=>{var v=_u();Ke(v,21,()=>n(r),rt,(E,S)=>{var j=mu(),re=l(j),ce=l(re),Ee=l(ce),$e=l(Ee),B=l($e),K=b($e,2),pe=l(K),se=b(Ee,2),Ae=l(se),U=b(ce,2),H=l(U),fe=b(re,2);{var ye=Y=>{var ie=hu(),Me=l(ie),Le=l(Me),je=b(Me,2);Ke(je,21,()=>n(S).tools,rt,(Oe,We)=>{var R=bu(),G=l(R),ue=l(G),Ue=b(G,2);{var Ie=ze=>{var T=pu(),L=l(T);M(()=>g(L,n(We).description)),h(ze,T)};ne(Ue,ze=>{n(We).description&&ze(Ie)})}M(()=>g(ue,n(We).name)),h(Oe,R)}),M(Oe=>g(Le,Oe),[()=>y("mcp.availableTools")]),h(Y,ie)},oe=Y=>{var ie=yu(),Me=l(ie),Le=l(Me);M(je=>g(Le,je),[()=>y("mcp.noTools")]),h(Y,ie)};ne(fe,Y=>{n(o)===n(S).name&&n(S).tools&&n(S).tools.length>0?Y(ye):n(o)===n(S).name&&(!n(S).tools||n(S).tools.length===0)&&Y(oe,1)})}M((Y,ie)=>{var Me;g(B,n(S).name),Be(K,1,`rounded-full px-2 py-1 text-xs font-medium ${n(S).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),g(pe,Y),g(Ae,n(S).url),g(H,`${((Me=n(S).tools)==null?void 0:Me.length)??0??""} ${ie??""}`)},[()=>n(S).status==="connected"?y("mcp.connected"):y("mcp.disconnected"),()=>y("mcp.tools")]),ae("click",re,()=>d(n(S).name)),h(E,j)}),h(w,v)};ne(F,w=>{n(a)?w($):n(s)?w(P,1):n(r).length===0?w(X,2):w(V,-1)})}M((w,v)=>{g(x,w),g(C,v)},[()=>y("mcp.title"),()=>y("common.refresh")]),ae("click",N,c),h(e,u),xe()}Xt(["click"]);var wu=A('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),Su=A("<div> </div>"),Au=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Eu=A('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),$u=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Mu=A('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),Cu=A('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),Pu=A('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),Tu=A('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <button type="button"><span></span></button></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between"><span> </span> <!></div></article>'),Nu=A('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Ou=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Iu=A('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Lu=A('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Ru=A('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),Fu=A('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),Du=A('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),ju=A('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),Uu=A('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),zu=A('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Hu=A('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option><option>ClawHub</option><option>HuggingFace</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),Bu=A('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function Vu(e,t){_e(t,!0);let r=O("installed"),a=O(ot([])),s=O(!0),o=O(""),i=O(""),d=O("success"),c=O(ot([])),u=O(!1),p=O(""),m=O("github"),x=O(!1),N=O(""),C=O(""),F=O("");function $(R,G="success"){f(i,R,!0),f(d,G,!0),setTimeout(()=>{f(i,"")},3e3)}async function P(){try{const R=await dt.getSkills();f(a,Array.isArray(R==null?void 0:R.skills)?R.skills:[],!0),f(o,"")}catch{f(a,[],!0),f(o,"Failed to load skills.")}finally{f(s,!1)}}async function X(R){try{await dt.toggleSkill(R),f(a,n(a).map(G=>G.name===R?{...G,enabled:!G.enabled}:G),!0)}catch{f(a,n(a).map(G=>G.name===R?{...G,enabled:!G.enabled}:G),!0)}}async function V(R){if(n(F)!==R){f(F,R,!0);return}f(F,""),f(C,R,!0);try{await dt.uninstallSkill(R),f(a,n(a).filter(G=>G.name!==R),!0),$(y("skills.uninstallSuccess"))}catch(G){$(y("skills.uninstallFailed")+(G.message?`: ${G.message}`:""),"error")}finally{f(C,"")}}const w=Ve(()=>[...n(a)].sort((R,G)=>R.enabled===G.enabled?0:R.enabled?-1:1)),v=Ve(()=>n(a).filter(R=>R.enabled).length);async function E(){!n(p).trim()&&n(m)==="github"&&f(p,"agent skill"),f(u,!0),f(x,!0);try{const R=await dt.discoverSkills(n(m),n(p));f(c,Array.isArray(R==null?void 0:R.results)?R.results:[],!0)}catch{f(c,[],!0)}finally{f(u,!1)}}function S(R){return n(a).some(G=>G.name===R)}async function j(R,G){f(N,R,!0);try{const ue=await dt.installSkill(R,G);ue!=null&&ue.skill&&f(a,[...n(a),{...ue.skill,enabled:!0}],!0),$(y("skills.installSuccess"))}catch(ue){$(y("skills.installFailed")+(ue.message?`: ${ue.message}`:""),"error")}finally{f(N,"")}}function re(R){R.key==="Enter"&&E()}Pt(()=>{P()});var ce=Bu(),Ee=l(ce),$e=l(Ee),B=l($e),K=l(B),pe=b(B,2);{var se=R=>{var G=wu(),ue=l(G);M(Ue=>g(ue,`${n(v)??""}/${n(a).length??""} ${Ue??""}`),[()=>y("skills.active")]),h(R,G)};ne(pe,R=>{!n(s)&&n(a).length>0&&R(se)})}var Ae=b($e,2),U=l(Ae),H=b(Ee,2),fe=l(H),ye=l(fe),oe=b(fe,2),Y=l(oe),ie=b(H,2);{var Me=R=>{var G=Su(),ue=l(G);M(()=>{Be(G,1,`rounded-lg px-4 py-2 text-sm ${n(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),g(ue,n(i))}),h(R,G)};ne(ie,R=>{n(i)&&R(Me)})}var Le=b(ie,2);{var je=R=>{var G=De(),ue=Ne(G);{var Ue=L=>{var q=Au(),Q=l(q);M(Z=>g(Q,Z),[()=>y("skills.loading")]),h(L,q)},Ie=L=>{var q=Eu(),Q=l(q);M(()=>g(Q,n(o))),h(L,q)},ze=L=>{var q=$u(),Q=l(q);M(Z=>g(Q,Z),[()=>y("skills.noSkills")]),h(L,q)},T=L=>{var q=Nu();Ke(q,21,()=>n(w),rt,(Q,Z)=>{var ke=Tu(),ee=l(ke),be=l(ee),de=l(be),ve=b(be,2),I=l(ve),W=b(ee,2);{var me=le=>{var J=Mu(),He=l(J);M(()=>g(He,n(Z).description)),h(le,J)};ne(W,le=>{n(Z).description&&le(me)})}var Ce=b(W,2),Re=l(Ce),qe=b(Ce,2),_=l(qe),z=l(_),D=b(_,2);{var te=le=>{var J=Cu(),He=l(J),gt=l(He),xt=b(He,2),Nt=l(xt),Ot=b(xt,2),at=l(Ot);M((nt,St,lr)=>{g(gt,nt),xt.disabled=n(C)===n(Z).name,g(Nt,St),g(at,lr)},[()=>y("skills.confirmUninstall").replace("{name}",n(Z).name),()=>n(C)===n(Z).name?y("skills.uninstalling"):y("common.yes"),()=>y("common.no")]),ae("click",xt,()=>V(n(Z).name)),ae("click",Ot,()=>{f(F,"")}),h(le,J)},Fe=le=>{var J=Pu(),He=l(J);M(gt=>g(He,gt),[()=>y("skills.uninstall")]),ae("click",J,()=>V(n(Z).name)),h(le,J)};ne(D,le=>{n(F)===n(Z).name?le(te):le(Fe,-1)})}M(le=>{g(de,n(Z).name),Be(ve,1,`relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition ${n(Z).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),Be(I,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${n(Z).enabled?"translate-x-4":"translate-x-1"}`),g(Re,n(Z).location),Be(_,1,`rounded-full px-2 py-1 text-xs font-medium ${n(Z).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),g(z,le)},[()=>n(Z).enabled?y("common.enabled"):y("common.disabled")]),ae("click",ve,()=>X(n(Z).name)),h(Q,ke)}),h(L,q)};ne(ue,L=>{n(s)?L(Ue):n(o)?L(Ie,1):n(a).length===0?L(ze,2):L(T,-1)})}h(R,G)};ne(Le,R=>{n(r)==="installed"&&R(je)})}var Oe=b(Le,2);{var We=R=>{var G=Hu(),ue=Ne(G),Ue=l(ue),Ie=l(Ue);Ie.value=Ie.__value="github";var ze=b(Ie);ze.value=ze.__value="clawhub";var T=b(ze);T.value=T.__value="huggingface";var L=b(Ue,2),q=b(L,2),Q=l(q),Z=b(ue,2);{var ke=de=>{var ve=Ou(),I=l(ve);M(W=>g(I,W),[()=>y("skills.searching")]),h(de,ve)},ee=de=>{var ve=Iu(),I=l(ve);M(W=>g(I,W),[()=>y("skills.noResults")]),h(de,ve)},be=de=>{var ve=zu();Ke(ve,21,()=>n(c),rt,(I,W)=>{const me=Ve(()=>S(n(W).name));var Ce=Uu(),Re=l(Ce),qe=l(Re),_=l(qe),z=l(_),D=b(_,2),te=l(D),Fe=b(qe,2),le=l(Fe),J=b(Re,2);{var He=Ze=>{var it=Lu(),It=l(it);M(()=>g(It,n(W).description)),h(Ze,it)};ne(J,Ze=>{n(W).description&&Ze(He)})}var gt=b(J,2),xt=l(gt);{var Nt=Ze=>{var it=Ru(),It=b(l(it));M(()=>g(It,` ${n(W).stars??""}`)),h(Ze,it)};ne(xt,Ze=>{n(W).stars>0&&Ze(Nt)})}var Ot=b(xt,2);{var at=Ze=>{var it=Fu(),It=l(it);M(()=>g(It,n(W).language)),h(Ze,it)};ne(Ot,Ze=>{n(W).language&&Ze(at)})}var nt=b(Ot,2),St=l(nt),lr=b(gt,2),dr=l(lr),cr=l(dr),ft=b(dr,2);{var kt=Ze=>{var it=Du(),It=l(it);M(pa=>g(It,pa),[()=>y("skills.installed")]),h(Ze,it)},en=Ze=>{var it=ju(),It=l(it);M(pa=>{it.disabled=n(N)===n(W).url,g(It,pa)},[()=>n(N)===n(W).url?y("skills.installing"):y("skills.install")]),ae("click",it,()=>j(n(W).url,n(W).name)),h(Ze,it)};ne(ft,Ze=>{n(me)?Ze(kt):Ze(en,-1)})}M((Ze,it,It)=>{g(z,n(W).name),g(te,`${Ze??""} ${n(W).owner??""}`),g(le,n(W).source),Be(nt,1,n(W).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400"),g(St,it),tt(dr,"href",n(W).url),g(cr,It)},[()=>y("skills.owner"),()=>n(W).has_license?y("skills.licensed"):y("skills.unlicensed"),()=>n(W).url.replace("https://github.com/","")]),h(I,Ce)}),h(de,ve)};ne(Z,de=>{n(u)?de(ke):n(x)&&n(c).length===0?de(ee,1):n(c).length>0&&de(be,2)})}M((de,ve)=>{tt(L,"placeholder",de),q.disabled=n(u),g(Q,ve)},[()=>y("skills.search"),()=>n(u)?y("skills.searching"):y("skills.searchBtn")]),$n(Ue,()=>n(m),de=>f(m,de)),ae("keydown",L,re),Lr(L,()=>n(p),de=>f(p,de)),ae("click",q,E),h(R,G)};ne(Oe,R=>{n(r)==="discover"&&R(We)})}M((R,G,ue,Ue)=>{g(K,R),g(U,G),Be(fe,1,`rounded-md px-4 py-2 text-sm font-medium transition ${n(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),g(ye,ue),Be(oe,1,`rounded-md px-4 py-2 text-sm font-medium transition ${n(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),g(Y,Ue)},[()=>y("skills.title"),()=>y("common.refresh"),()=>y("skills.tabInstalled"),()=>y("skills.tabDiscover")]),ae("click",Ae,()=>{f(s,!0),P()}),ae("click",fe,()=>{f(r,"installed")}),ae("click",oe,()=>{f(r,"discover")}),h(e,ce),xe()}Xt(["click","keydown"]);var Wu=A("<div> </div>"),qu=A('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Ku=A('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),Gu=A('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Ju=A('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),Yu=A('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),Xu=A('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),Qu=A('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),Zu=A('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),ef=A('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),tf=A('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),rf=A('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function af(e,t){_e(t,!0);let r=O(ot([])),a=O(!0),s=O(""),o=O(""),i=O(""),d=O("success");function c(B,K="success"){f(i,B,!0),f(d,K,!0),setTimeout(()=>{f(i,"")},3e3)}async function u(){f(a,!0);try{const B=await dt.getPlugins();f(r,Array.isArray(B==null?void 0:B.plugins)?B.plugins:[],!0),f(s,"")}catch{f(r,[],!0),f(s,y("plugins.loadFailed"),!0)}finally{f(a,!1)}}async function p(B){f(o,B,!0);try{await dt.reloadPlugin(B),c(y("plugins.reloadSuccess",{name:B})),await u()}catch(K){c(y("plugins.reloadFailed")+(K.message?`: ${K.message}`:""),"error")}finally{f(o,"")}}function m(B){return typeof B=="string"&&B==="Active"?"text-green-500":typeof B=="object"&&(B!=null&&B.Error)?"text-red-500":"text-yellow-500"}function x(B){return typeof B=="string"&&B==="Active"?y("plugins.statusActive"):typeof B=="object"&&(B!=null&&B.Error)?B.Error:y("common.unknown")}Pt(()=>{u()});var N=rf(),C=Ne(N);{var F=B=>{var K=Wu(),pe=l(K);M(()=>{Be(K,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${n(d)==="error"?"bg-red-600":"bg-green-600"}`),g(pe,n(i))}),h(B,K)};ne(C,B=>{n(i)&&B(F)})}var $=b(C,2),P=l($),X=l(P),V=l(X);us(V,{size:24});var w=b(V,2),v=l(w),E=b(X,2),S=l(E),j=b(P,2);{var re=B=>{var K=qu(),pe=l(K);M(se=>g(pe,se),[()=>y("plugins.loading")]),h(B,K)},ce=B=>{var K=Ku(),pe=l(K);M(()=>g(pe,n(s))),h(B,K)},Ee=B=>{var K=Gu(),pe=l(K);us(pe,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var se=b(pe,2),Ae=l(se);M(U=>g(Ae,U),[()=>y("plugins.noPlugins")]),h(B,K)},$e=B=>{var K=tf();Ke(K,21,()=>n(r),rt,(pe,se)=>{var Ae=ef(),U=l(Ae),H=l(U),fe=l(H),ye=l(fe),oe=b(fe,2),Y=l(oe),ie=b(H,2),Me=l(ie);{var Le=ee=>{bd(ee,{size:16})},je=ee=>{pd(ee,{size:16})};ne(Me,ee=>{typeof n(se).status=="string"&&n(se).status==="Active"?ee(Le):ee(je,-1)})}var Oe=b(Me,2),We=l(Oe),R=b(U,2);{var G=ee=>{var be=Ju(),de=l(be);M(()=>g(de,n(se).description)),h(ee,be)};ne(R,ee=>{n(se).description&&ee(G)})}var ue=b(R,2);{var Ue=ee=>{var be=Xu(),de=l(be),ve=l(de),I=b(de,2);Ke(I,21,()=>n(se).capabilities,rt,(W,me)=>{var Ce=Yu(),Re=l(Ce);M(()=>g(Re,n(me))),h(W,Ce)}),M(W=>g(ve,W),[()=>y("plugins.capabilities")]),h(ee,be)};ne(ue,ee=>{var be;(be=n(se).capabilities)!=null&&be.length&&ee(Ue)})}var Ie=b(ue,2);{var ze=ee=>{var be=Zu(),de=l(be),ve=l(de),I=b(de,2);Ke(I,21,()=>n(se).permissions_required,rt,(W,me)=>{var Ce=Qu(),Re=l(Ce);M(()=>g(Re,n(me))),h(W,Ce)}),M(W=>g(ve,W),[()=>y("plugins.permissions")]),h(ee,be)};ne(Ie,ee=>{var be;(be=n(se).permissions_required)!=null&&be.length&&ee(ze)})}var T=b(Ie,2),L=l(T),q=l(L);{var Q=ee=>{kd(ee,{size:14,class:"animate-spin"})},Z=ee=>{_o(ee,{size:14})};ne(q,ee=>{n(o)===n(se).name?ee(Q):ee(Z,-1)})}var ke=b(q);M((ee,be,de)=>{g(ye,n(se).name),g(Y,`v${n(se).version??""}`),Be(ie,1,`flex items-center gap-1 ${ee??""}`),g(We,be),L.disabled=n(o)===n(se).name,g(ke,` ${de??""}`)},[()=>m(n(se).status),()=>x(n(se).status),()=>y("plugins.reload")]),ae("click",L,()=>p(n(se).name)),h(pe,Ae)}),h(B,K)};ne(j,B=>{n(a)?B(re):n(s)?B(ce,1):n(r).length===0?B(Ee,2):B($e,-1)})}M((B,K)=>{g(v,B),g(S,K)},[()=>y("plugins.title"),()=>y("common.refresh")]),ae("click",E,u),h(e,N),xe()}Xt(["click"]);var nf=A('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),sf=A('<button type="button"> </button>'),of=A('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),lf=A('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),df=A('<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function cf(e,t){_e(t,!0);let r=O(ot(mo())),a=O(ot(Ma())),s=O(!1),o=O(!0);const i=Ve(()=>n(a).length>0),d=Ve(()=>n(i)&&n(r)==="/"?"/overview":n(r)),c=Ve(()=>n(d).startsWith("/chat/")?"/sessions":n(d));function u(S){try{return decodeURIComponent(S)}catch{return S}}const p=Ve(()=>n(d).startsWith("/chat/")?u(n(d).slice(6)):"");function m(){localStorage.getItem("prx-console-theme")==="light"?f(o,!1):f(o,!0),x()}function x(){n(o)?document.documentElement.classList.add("dark"):document.documentElement.classList.remove("dark")}function N(){f(o,!n(o)),localStorage.setItem("prx-console-theme",n(o)?"dark":"light"),x()}function C(){f(a,Ma(),!0)}function F(S){f(r,S,!0),f(s,!1)}function $(S){f(a,S,!0),yr("/overview",!0)}function P(){bo(),f(a,""),yr("/",!0)}function X(S){yr(S)}Pt(()=>{m();const S=od(F),j=re=>{if(re.key==="prx-console-token"){C();return}if(re.key===Za&&sd(),re.key==="prx-console-theme"){const ce=localStorage.getItem("prx-console-theme");f(o,ce!=="light"),x()}};return window.addEventListener("storage",j),()=>{S(),window.removeEventListener("storage",j)}}),Pt(()=>{if(n(i)&&n(r)==="/"){yr("/overview",!0);return}!n(i)&&n(r)!=="/"&&yr("/",!0)});var V=df(),w=l(V);{var v=S=>{Od(S,{onLogin:$})},E=S=>{var j=lf(),re=l(j);{var ce=I=>{var W=nf();M(me=>tt(W,"aria-label",me),[()=>y("app.closeSidebar")]),ae("click",W,()=>f(s,!1)),h(I,W)};ne(re,I=>{n(s)&&I(ce)})}var Ee=b(re,2),$e=l(Ee),B=l($e),K=l(B),pe=b($e,2);Ke(pe,21,()=>Al,rt,(I,W)=>{var me=sf(),Ce=l(me);M(Re=>{Be(me,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${n(c)===n(W).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),g(Ce,Re)},[()=>y(n(W).labelKey)]),ae("click",me,()=>X(n(W).path)),h(I,me)});var se=b(Ee,2),Ae=l(se),U=l(Ae),H=l(U),fe=l(H),ye=b(H,2),oe=l(ye),Y=b(U,2),ie=l(Y),Me=l(ie);{var Le=I=>{Cd(I,{size:16})},je=I=>{Sd(I,{size:16})};ne(Me,I=>{n(o)?I(Le):I(je,-1)})}var Oe=b(ie,2),We=l(Oe),R=b(Oe,2),G=l(R),ue=b(Ae,2),Ue=l(ue);{var Ie=I=>{qd(I,{})},ze=I=>{ec(I,{})},T=I=>{bc(I,{get sessionId(){return n(p)}})},L=Ve(()=>n(d).startsWith("/chat/")),q=I=>{Sc(I,{})},Q=I=>{uu(I,{})},Z=I=>{ku(I,{})},ke=I=>{Vu(I,{})},ee=I=>{af(I,{})},be=I=>{Gc(I,{})},de=I=>{Zc(I,{})},ve=I=>{var W=of(),me=l(W),Ce=l(me),Re=b(me,2),qe=l(Re);M((_,z)=>{g(Ce,_),g(qe,z)},[()=>y("app.notFound"),()=>y("app.backToOverview")]),ae("click",Re,()=>X("/overview")),h(I,W)};ne(Ue,I=>{n(d)==="/overview"?I(Ie):n(d)==="/sessions"?I(ze,1):n(L)?I(T,2):n(d)==="/channels"?I(q,3):n(d)==="/hooks"?I(Q,4):n(d)==="/mcp"?I(Z,5):n(d)==="/skills"?I(ke,6):n(d)==="/plugins"?I(ee,7):n(d)==="/config"?I(be,8):n(d)==="/logs"?I(de,9):I(ve,-1)})}M((I,W,me,Ce,Re)=>{Be(Ee,1,`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${n(s)?"translate-x-0":"-translate-x-full"}`),g(K,I),g(fe,W),g(oe,me),tt(Oe,"aria-label",Ce),g(We,Vr.lang==="zh"?"中文 / EN":"EN / 中文"),g(G,Re)},[()=>y("app.title"),()=>y("app.menu"),()=>y("app.title"),()=>y("app.language"),()=>y("common.logout")]),ae("click",H,()=>f(s,!n(s))),ae("click",ie,N),ae("click",Oe,function(...I){Gr==null||Gr.apply(this,I)}),ae("click",R,P),h(S,j)};ne(w,S=>{n(i)?S(E,-1):S(v)})}h(e,V),xe()}Xt(["click"]);el(cf,{target:document.getElementById("app")});
