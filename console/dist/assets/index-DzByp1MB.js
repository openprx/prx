var Io=Object.defineProperty;var ss=e=>{throw TypeError(e)};var Lo=(e,t,r)=>t in e?Io(e,t,{enumerable:!0,configurable:!0,writable:!0,value:r}):e[t]=r;var sr=(e,t,r)=>Lo(e,typeof t!="symbol"?t+"":t,r),yn=(e,t,r)=>t.has(e)||ss("Cannot "+r);var A=(e,t,r)=>(yn(e,t,"read from private field"),r?r.call(e):t.get(e)),Ve=(e,t,r)=>t.has(e)?ss("Cannot add the same private member more than once"):t instanceof WeakSet?t.add(e):t.set(e,r),Pe=(e,t,r,n)=>(yn(e,t,"write to private field"),n?n.call(e,r):t.set(e,r),r),Tt=(e,t,r)=>(yn(e,t,"access private method"),r);(function(){const t=document.createElement("link").relList;if(t&&t.supports&&t.supports("modulepreload"))return;for(const s of document.querySelectorAll('link[rel="modulepreload"]'))n(s);new MutationObserver(s=>{for(const o of s)if(o.type==="childList")for(const l of o.addedNodes)l.tagName==="LINK"&&l.rel==="modulepreload"&&n(l)}).observe(document,{childList:!0,subtree:!0});function r(s){const o={};return s.integrity&&(o.integrity=s.integrity),s.referrerPolicy&&(o.referrerPolicy=s.referrerPolicy),s.crossOrigin==="use-credentials"?o.credentials="include":s.crossOrigin==="anonymous"?o.credentials="omit":o.credentials="same-origin",o}function n(s){if(s.ep)return;s.ep=!0;const o=r(s);fetch(s.href,o)}})();const En=!1;var Wn=Array.isArray,Fo=Array.prototype.indexOf,ha=Array.prototype.includes,sn=Array.from,Ro=Object.defineProperty,Rr=Object.getOwnPropertyDescriptor,jo=Object.getOwnPropertyDescriptors,Do=Object.prototype,Ho=Array.prototype,Cs=Object.getPrototypeOf,os=Object.isExtensible;function Ta(e){return typeof e=="function"}const we=()=>{};function zo(e){for(var t=0;t<e.length;t++)e[t]()}function Ms(){var e,t,r=new Promise((n,s)=>{e=n,t=s});return{promise:r,resolve:e,reject:t}}function La(e,t){if(Array.isArray(e))return e;if(!(Symbol.iterator in e))return Array.from(e);const r=[];for(const n of e)if(r.push(n),r.length===t)break;return r}const Dt=2,Aa=4,ma=8,on=1<<24,Br=16,ur=32,na=64,$n=128,rr=512,Ft=1024,Rt=2048,cr=4096,Ut=8192,mr=16384,Ea=32768,Mr=65536,is=1<<17,Uo=1<<18,$a=1<<19,Bo=1<<20,yr=1<<25,ta=65536,Cn=1<<21,Vn=1<<22,jr=1<<23,Dr=Symbol("$state"),Ns=Symbol("legacy props"),Wo=Symbol(""),Wr=new class extends Error{constructor(){super(...arguments);sr(this,"name","StaleReactionError");sr(this,"message","The reaction that called `getAbortSignal()` was re-run or destroyed")}};var As;const qn=!!((As=globalThis.document)!=null&&As.contentType)&&globalThis.document.contentType.includes("xml");function Ts(e){throw new Error("https://svelte.dev/e/lifecycle_outside_component")}function Vo(){throw new Error("https://svelte.dev/e/async_derived_orphan")}function qo(e,t,r){throw new Error("https://svelte.dev/e/each_key_duplicate")}function Go(e){throw new Error("https://svelte.dev/e/effect_in_teardown")}function Ko(){throw new Error("https://svelte.dev/e/effect_in_unowned_derived")}function Jo(e){throw new Error("https://svelte.dev/e/effect_orphan")}function Yo(){throw new Error("https://svelte.dev/e/effect_update_depth_exceeded")}function Xo(e){throw new Error("https://svelte.dev/e/props_invalid_value")}function Qo(){throw new Error("https://svelte.dev/e/state_descriptors_fixed")}function Zo(){throw new Error("https://svelte.dev/e/state_prototype_fixed")}function ei(){throw new Error("https://svelte.dev/e/state_unsafe_mutation")}function ti(){throw new Error("https://svelte.dev/e/svelte_boundary_reset_onerror")}const ri=1,ai=2,Ps=4,ni=8,si=16,oi=1,ii=4,li=8,di=16,ci=1,ui=2,Ot=Symbol(),Os="http://www.w3.org/1999/xhtml",Is="http://www.w3.org/2000/svg",fi="http://www.w3.org/1998/Math/MathML",vi="@attach";function gi(){console.warn("https://svelte.dev/e/select_multiple_invalid_value")}function pi(){console.warn("https://svelte.dev/e/svelte_boundary_reset_noop")}function Ls(e){return e===this.v}function bi(e,t){return e!=e?t==t:e!==t||e!==null&&typeof e=="object"||typeof e=="function"}function Fs(e){return!bi(e,this.v)}let yi=!1,Jt=null;function _a(e){Jt=e}function Ce(e,t=!1,r){Jt={p:Jt,i:!1,c:null,e:null,s:e,x:null,l:null}}function Me(e){var t=Jt,r=t.e;if(r!==null){t.e=null;for(var n of r)ao(n)}return t.i=!0,Jt=t.p,{}}function Rs(){return!0}let Vr=[];function js(){var e=Vr;Vr=[],zo(e)}function _r(e){if(Vr.length===0&&!Ra){var t=Vr;queueMicrotask(()=>{t===Vr&&js()})}Vr.push(e)}function hi(){for(;Vr.length>0;)js()}function Ds(e){var t=Qe;if(t===null)return He.f|=jr,e;if(!(t.f&Ea)&&!(t.f&Aa))throw e;Fr(e,t)}function Fr(e,t){for(;t!==null;){if(t.f&$n){if(!(t.f&Ea))throw e;try{t.b.error(e);return}catch(r){e=r}}t=t.parent}throw e}const mi=-7169;function At(e,t){e.f=e.f&mi|t}function Gn(e){e.f&rr||e.deps===null?At(e,Ft):At(e,cr)}function Hs(e){if(e!==null)for(const t of e)!(t.f&Dt)||!(t.f&ta)||(t.f^=ta,Hs(t.deps))}function zs(e,t,r){e.f&Rt?t.add(e):e.f&cr&&r.add(e),Hs(e.deps),At(e,Ft)}const Ja=new Set;let Se=null,tn=null,Lt=null,qt=[],ln=null,Ra=!1,xa=null,_i=1;var Or,ca,Jr,ua,fa,va,Ir,vr,ga,Yt,Mn,Nn,Tn,Pn;const ns=class ns{constructor(){Ve(this,Yt);sr(this,"id",_i++);sr(this,"current",new Map);sr(this,"previous",new Map);Ve(this,Or,new Set);Ve(this,ca,new Set);Ve(this,Jr,0);Ve(this,ua,0);Ve(this,fa,null);Ve(this,va,new Set);Ve(this,Ir,new Set);Ve(this,vr,new Map);sr(this,"is_fork",!1);Ve(this,ga,!1)}skip_effect(t){A(this,vr).has(t)||A(this,vr).set(t,{d:[],m:[]})}unskip_effect(t){var r=A(this,vr).get(t);if(r){A(this,vr).delete(t);for(var n of r.d)At(n,Rt),hr(n);for(n of r.m)At(n,cr),hr(n)}}process(t){var s;qt=[],this.apply();var r=xa=[],n=[];for(const o of t)Tt(this,Yt,Nn).call(this,o,r,n);if(xa=null,Tt(this,Yt,Mn).call(this)){Tt(this,Yt,Tn).call(this,n),Tt(this,Yt,Tn).call(this,r);for(const[o,l]of A(this,vr))Vs(o,l)}else{tn=this,Se=null;for(const o of A(this,Or))o(this);A(this,Or).clear(),A(this,Jr)===0&&Tt(this,Yt,Pn).call(this),ls(n),ls(r),A(this,va).clear(),A(this,Ir).clear(),tn=null,(s=A(this,fa))==null||s.resolve()}Lt=null}capture(t,r){r!==Ot&&!this.previous.has(t)&&this.previous.set(t,r),t.f&jr||(this.current.set(t,t.v),Lt==null||Lt.set(t,t.v))}activate(){Se=this,this.apply()}deactivate(){Se===this&&(Se=null,Lt=null)}flush(){var t;if(qt.length>0)Se=this,Us();else if(A(this,Jr)===0&&!this.is_fork){for(const r of A(this,Or))r(this);A(this,Or).clear(),Tt(this,Yt,Pn).call(this),(t=A(this,fa))==null||t.resolve()}this.deactivate()}discard(){for(const t of A(this,ca))t(this);A(this,ca).clear()}increment(t){Pe(this,Jr,A(this,Jr)+1),t&&Pe(this,ua,A(this,ua)+1)}decrement(t){Pe(this,Jr,A(this,Jr)-1),t&&Pe(this,ua,A(this,ua)-1),!A(this,ga)&&(Pe(this,ga,!0),_r(()=>{Pe(this,ga,!1),Tt(this,Yt,Mn).call(this)?qt.length>0&&this.flush():this.revive()}))}revive(){for(const t of A(this,va))A(this,Ir).delete(t),At(t,Rt),hr(t);for(const t of A(this,Ir))At(t,cr),hr(t);this.flush()}oncommit(t){A(this,Or).add(t)}ondiscard(t){A(this,ca).add(t)}settled(){return(A(this,fa)??Pe(this,fa,Ms())).promise}static ensure(){if(Se===null){const t=Se=new ns;Ja.add(Se),Ra||_r(()=>{Se===t&&t.flush()})}return Se}apply(){}};Or=new WeakMap,ca=new WeakMap,Jr=new WeakMap,ua=new WeakMap,fa=new WeakMap,va=new WeakMap,Ir=new WeakMap,vr=new WeakMap,ga=new WeakMap,Yt=new WeakSet,Mn=function(){return this.is_fork||A(this,ua)>0},Nn=function(t,r,n){t.f^=Ft;for(var s=t.first;s!==null;){var o=s.f,l=(o&(ur|na))!==0,d=l&&(o&Ft)!==0,c=(o&Ut)!==0,u=d||A(this,vr).has(s);if(!u&&s.fn!==null){l?c||(s.f^=Ft):o&Aa?r.push(s):o&(ma|on)&&c?n.push(s):Ga(s)&&(wa(s),o&Br&&(A(this,Ir).add(s),c&&At(s,Rt)));var y=s.first;if(y!==null){s=y;continue}}for(;s!==null;){var m=s.next;if(m!==null){s=m;break}s=s.parent}}},Tn=function(t){for(var r=0;r<t.length;r+=1)zs(t[r],A(this,va),A(this,Ir))},Pn=function(){var o;if(Ja.size>1){this.previous.clear();var t=Se,r=Lt,n=!0;for(const l of Ja){if(l===this){n=!1;continue}const d=[];for(const[u,y]of this.current){if(l.current.has(u))if(n&&y!==l.current.get(u))l.current.set(u,y);else continue;d.push(u)}if(d.length===0)continue;const c=[...l.current.keys()].filter(u=>!this.current.has(u));if(c.length>0){var s=qt;qt=[];const u=new Set,y=new Map;for(const m of d)Bs(m,c,u,y);if(qt.length>0){Se=l,l.apply();for(const m of qt)Tt(o=l,Yt,Nn).call(o,m,[],[]);l.deactivate()}qt=s}}Se=t,Lt=r}A(this,vr).clear(),Ja.delete(this)};let Hr=ns;function xi(e){var t=Ra;Ra=!0;try{for(var r;;){if(hi(),qt.length===0&&(Se==null||Se.flush(),qt.length===0))return ln=null,r;Us()}}finally{Ra=t}}function Us(){var e=null;try{for(var t=0;qt.length>0;){var r=Hr.ensure();if(t++>1e3){var n,s;ki()}r.process(qt),zr.clear()}}finally{qt=[],ln=null,xa=null}}function ki(){try{Yo()}catch(e){Fr(e,ln)}}let or=null;function ls(e){var t=e.length;if(t!==0){for(var r=0;r<t;){var n=e[r++];if(!(n.f&(mr|Ut))&&Ga(n)&&(or=new Set,wa(n),n.deps===null&&n.first===null&&n.nodes===null&&n.teardown===null&&n.ac===null&&io(n),(or==null?void 0:or.size)>0)){zr.clear();for(const s of or){if(s.f&(mr|Ut))continue;const o=[s];let l=s.parent;for(;l!==null;)or.has(l)&&(or.delete(l),o.push(l)),l=l.parent;for(let d=o.length-1;d>=0;d--){const c=o[d];c.f&(mr|Ut)||wa(c)}}or.clear()}}or=null}}function Bs(e,t,r,n){if(!r.has(e)&&(r.add(e),e.reactions!==null))for(const s of e.reactions){const o=s.f;o&Dt?Bs(s,t,r,n):o&(Vn|Br)&&!(o&Rt)&&Ws(s,t,n)&&(At(s,Rt),hr(s))}}function Ws(e,t,r){const n=r.get(e);if(n!==void 0)return n;if(e.deps!==null)for(const s of e.deps){if(ha.call(t,s))return!0;if(s.f&Dt&&Ws(s,t,r))return r.set(s,!0),!0}return r.set(e,!1),!1}function hr(e){var t=ln=e,r=t.b;if(r!=null&&r.is_pending&&e.f&(Aa|ma|on)&&!(e.f&Ea)){r.defer_effect(e);return}for(;t.parent!==null;){t=t.parent;var n=t.f;if(xa!==null&&t===Qe&&!(e.f&ma))return;if(n&(na|ur)){if(!(n&Ft))return;t.f^=Ft}}qt.push(t)}function Vs(e,t){if(!(e.f&ur&&e.f&Ft)){e.f&Rt?t.d.push(e):e.f&cr&&t.m.push(e),At(e,Ft);for(var r=e.first;r!==null;)Vs(r,t),r=r.next}}function wi(e){let t=0,r=ra(0),n;return()=>{Yn()&&(a(r),Xn(()=>(t===0&&(n=Ma(()=>e(()=>ja(r)))),t+=1,()=>{_r(()=>{t-=1,t===0&&(n==null||n(),n=void 0,ja(r))})})))}}var Si=Mr|$a;function Ai(e,t,r,n){new Ei(e,t,r,n)}var tr,Bn,gr,Yr,Vt,pr,Qt,ir,Sr,Xr,Lr,pa,ba,ya,Ar,an,Pt,$i,Ci,Mi,On,Qa,Za,In;class Ei{constructor(t,r,n,s){Ve(this,Pt);sr(this,"parent");sr(this,"is_pending",!1);sr(this,"transform_error");Ve(this,tr);Ve(this,Bn,null);Ve(this,gr);Ve(this,Yr);Ve(this,Vt);Ve(this,pr,null);Ve(this,Qt,null);Ve(this,ir,null);Ve(this,Sr,null);Ve(this,Xr,0);Ve(this,Lr,0);Ve(this,pa,!1);Ve(this,ba,new Set);Ve(this,ya,new Set);Ve(this,Ar,null);Ve(this,an,wi(()=>(Pe(this,Ar,ra(A(this,Xr))),()=>{Pe(this,Ar,null)})));var o;Pe(this,tr,t),Pe(this,gr,r),Pe(this,Yr,l=>{var d=Qe;d.b=this,d.f|=$n,n(l)}),this.parent=Qe.b,this.transform_error=s??((o=this.parent)==null?void 0:o.transform_error)??(l=>l),Pe(this,Vt,Ca(()=>{Tt(this,Pt,On).call(this)},Si))}defer_effect(t){zs(t,A(this,ba),A(this,ya))}is_rendered(){return!this.is_pending&&(!this.parent||this.parent.is_rendered())}has_pending_snippet(){return!!A(this,gr).pending}update_pending_count(t){Tt(this,Pt,In).call(this,t),Pe(this,Xr,A(this,Xr)+t),!(!A(this,Ar)||A(this,pa))&&(Pe(this,pa,!0),_r(()=>{Pe(this,pa,!1),A(this,Ar)&&ka(A(this,Ar),A(this,Xr))}))}get_effect_pending(){return A(this,an).call(this),a(A(this,Ar))}error(t){var r=A(this,gr).onerror;let n=A(this,gr).failed;if(!r&&!n)throw t;A(this,pr)&&(jt(A(this,pr)),Pe(this,pr,null)),A(this,Qt)&&(jt(A(this,Qt)),Pe(this,Qt,null)),A(this,ir)&&(jt(A(this,ir)),Pe(this,ir,null));var s=!1,o=!1;const l=()=>{if(s){pi();return}s=!0,o&&ti(),A(this,ir)!==null&&Zr(A(this,ir),()=>{Pe(this,ir,null)}),Tt(this,Pt,Za).call(this,()=>{Hr.ensure(),Tt(this,Pt,On).call(this)})},d=c=>{try{o=!0,r==null||r(c,l),o=!1}catch(u){Fr(u,A(this,Vt)&&A(this,Vt).parent)}n&&Pe(this,ir,Tt(this,Pt,Za).call(this,()=>{Hr.ensure();try{return Kt(()=>{var u=Qe;u.b=this,u.f|=$n,n(A(this,tr),()=>c,()=>l)})}catch(u){return Fr(u,A(this,Vt).parent),null}}))};_r(()=>{var c;try{c=this.transform_error(t)}catch(u){Fr(u,A(this,Vt)&&A(this,Vt).parent);return}c!==null&&typeof c=="object"&&typeof c.then=="function"?c.then(d,u=>Fr(u,A(this,Vt)&&A(this,Vt).parent)):d(c)})}}tr=new WeakMap,Bn=new WeakMap,gr=new WeakMap,Yr=new WeakMap,Vt=new WeakMap,pr=new WeakMap,Qt=new WeakMap,ir=new WeakMap,Sr=new WeakMap,Xr=new WeakMap,Lr=new WeakMap,pa=new WeakMap,ba=new WeakMap,ya=new WeakMap,Ar=new WeakMap,an=new WeakMap,Pt=new WeakSet,$i=function(){try{Pe(this,pr,Kt(()=>A(this,Yr).call(this,A(this,tr))))}catch(t){this.error(t)}},Ci=function(t){const r=A(this,gr).failed;r&&Pe(this,ir,Kt(()=>{r(A(this,tr),()=>t,()=>()=>{})}))},Mi=function(){const t=A(this,gr).pending;t&&(this.is_pending=!0,Pe(this,Qt,Kt(()=>t(A(this,tr)))),_r(()=>{var r=Pe(this,Sr,document.createDocumentFragment()),n=$r();r.append(n),Pe(this,pr,Tt(this,Pt,Za).call(this,()=>(Hr.ensure(),Kt(()=>A(this,Yr).call(this,n))))),A(this,Lr)===0&&(A(this,tr).before(r),Pe(this,Sr,null),Zr(A(this,Qt),()=>{Pe(this,Qt,null)}),Tt(this,Pt,Qa).call(this))}))},On=function(){try{if(this.is_pending=this.has_pending_snippet(),Pe(this,Lr,0),Pe(this,Xr,0),Pe(this,pr,Kt(()=>{A(this,Yr).call(this,A(this,tr))})),A(this,Lr)>0){var t=Pe(this,Sr,document.createDocumentFragment());es(A(this,pr),t);const r=A(this,gr).pending;Pe(this,Qt,Kt(()=>r(A(this,tr))))}else Tt(this,Pt,Qa).call(this)}catch(r){this.error(r)}},Qa=function(){this.is_pending=!1;for(const t of A(this,ba))At(t,Rt),hr(t);for(const t of A(this,ya))At(t,cr),hr(t);A(this,ba).clear(),A(this,ya).clear()},Za=function(t){var r=Qe,n=He,s=Jt;xr(A(this,Vt)),nr(A(this,Vt)),_a(A(this,Vt).ctx);try{return t()}catch(o){return Ds(o),null}finally{xr(r),nr(n),_a(s)}},In=function(t){var r;if(!this.has_pending_snippet()){this.parent&&Tt(r=this.parent,Pt,In).call(r,t);return}Pe(this,Lr,A(this,Lr)+t),A(this,Lr)===0&&(Tt(this,Pt,Qa).call(this),A(this,Qt)&&Zr(A(this,Qt),()=>{Pe(this,Qt,null)}),A(this,Sr)&&(A(this,tr).before(A(this,Sr)),Pe(this,Sr,null)))};function qs(e,t,r,n){const s=dn;var o=e.filter(m=>!m.settled);if(r.length===0&&o.length===0){n(t.map(s));return}var l=Qe,d=Ni(),c=o.length===1?o[0].promise:o.length>1?Promise.all(o.map(m=>m.promise)):null;function u(m){d();try{n(m)}catch(k){l.f&mr||Fr(k,l)}Ln()}if(r.length===0){c.then(()=>u(t.map(s)));return}function y(){d(),Promise.all(r.map(m=>Pi(m))).then(m=>u([...t.map(s),...m])).catch(m=>Fr(m,l))}c?c.then(y):y()}function Ni(){var e=Qe,t=He,r=Jt,n=Se;return function(o=!0){xr(e),nr(t),_a(r),o&&(n==null||n.activate())}}function Ln(e=!0){xr(null),nr(null),_a(null),e&&(Se==null||Se.deactivate())}function Ti(){var e=Qe.b,t=Se,r=e.is_rendered();return e.update_pending_count(1),t.increment(r),()=>{e.update_pending_count(-1),t.decrement(r)}}function dn(e){var t=Dt|Rt,r=He!==null&&He.f&Dt?He:null;return Qe!==null&&(Qe.f|=$a),{ctx:Jt,deps:null,effects:null,equals:Ls,f:t,fn:e,reactions:null,rv:0,v:Ot,wv:0,parent:r??Qe,ac:null}}function Pi(e,t,r){Qe===null&&Vo();var s=void 0,o=ra(Ot),l=!He,d=new Map;return qi(()=>{var k;var c=Ms();s=c.promise;try{Promise.resolve(e()).then(c.resolve,c.reject).finally(Ln)}catch(I){c.reject(I),Ln()}var u=Se;if(l){var y=Ti();(k=d.get(u))==null||k.reject(Wr),d.delete(u),d.set(u,c)}const m=(I,T=void 0)=>{if(u.activate(),T)T!==Wr&&(o.f|=jr,ka(o,T));else{o.f&jr&&(o.f^=jr),ka(o,I);for(const[F,M]of d){if(d.delete(F),F===u)break;M.reject(Wr)}}y&&y()};c.promise.then(m,I=>m(null,I||"unknown"))}),un(()=>{for(const c of d.values())c.reject(Wr)}),new Promise(c=>{function u(y){function m(){y===s?c(o):u(s)}y.then(m,m)}u(s)})}function ne(e){const t=dn(e);return uo(t),t}function Gs(e){const t=dn(e);return t.equals=Fs,t}function Oi(e){var t=e.effects;if(t!==null){e.effects=null;for(var r=0;r<t.length;r+=1)jt(t[r])}}function Ii(e){for(var t=e.parent;t!==null;){if(!(t.f&Dt))return t.f&mr?null:t;t=t.parent}return null}function Kn(e){var t,r=Qe;xr(Ii(e));try{e.f&=~ta,Oi(e),t=po(e)}finally{xr(r)}return t}function Ks(e){var t=Kn(e);if(!e.equals(t)&&(e.wv=vo(),(!(Se!=null&&Se.is_fork)||e.deps===null)&&(e.v=t,e.deps===null))){At(e,Ft);return}Ur||(Lt!==null?(Yn()||Se!=null&&Se.is_fork)&&Lt.set(e,t):Gn(e))}function Li(e){var t,r;if(e.effects!==null)for(const n of e.effects)(n.teardown||n.ac)&&((t=n.teardown)==null||t.call(n),(r=n.ac)==null||r.abort(Wr),n.teardown=we,n.ac=null,Ha(n,0),Qn(n))}function Js(e){if(e.effects!==null)for(const t of e.effects)t.teardown&&wa(t)}let Fn=new Set;const zr=new Map;let Ys=!1;function ra(e,t){var r={f:0,v:e,reactions:null,equals:Ls,rv:0,wv:0};return r}function L(e,t){const r=ra(e);return uo(r),r}function Fi(e,t=!1,r=!0){const n=ra(e);return t||(n.equals=Fs),n}function b(e,t,r=!1){He!==null&&(!dr||He.f&is)&&Rs()&&He.f&(Dt|Br|Vn|is)&&(ar===null||!ha.call(ar,e))&&ei();let n=r?Et(t):t;return ka(e,n)}function ka(e,t){if(!e.equals(t)){var r=e.v;Ur?zr.set(e,t):zr.set(e,r),e.v=t;var n=Hr.ensure();if(n.capture(e,r),e.f&Dt){const s=e;e.f&Rt&&Kn(s),Gn(s)}e.wv=vo(),Xs(e,Rt),Qe!==null&&Qe.f&Ft&&!(Qe.f&(ur|na))&&(er===null?Ki([e]):er.push(e)),!n.is_fork&&Fn.size>0&&!Ys&&Ri()}return t}function Ri(){Ys=!1;for(const e of Fn)e.f&Ft&&At(e,cr),Ga(e)&&wa(e);Fn.clear()}function ja(e){b(e,e.v+1)}function Xs(e,t){var r=e.reactions;if(r!==null)for(var n=r.length,s=0;s<n;s++){var o=r[s],l=o.f,d=(l&Rt)===0;if(d&&At(o,t),l&Dt){var c=o;Lt==null||Lt.delete(c),l&ta||(l&rr&&(o.f|=ta),Xs(c,cr))}else d&&(l&Br&&or!==null&&or.add(o),hr(o))}}function Et(e){if(typeof e!="object"||e===null||Dr in e)return e;const t=Cs(e);if(t!==Do&&t!==Ho)return e;var r=new Map,n=Wn(e),s=L(0),o=ea,l=d=>{if(ea===o)return d();var c=He,u=ea;nr(null),vs(o);var y=d();return nr(c),vs(u),y};return n&&r.set("length",L(e.length)),new Proxy(e,{defineProperty(d,c,u){(!("value"in u)||u.configurable===!1||u.enumerable===!1||u.writable===!1)&&Qo();var y=r.get(c);return y===void 0?l(()=>{var m=L(u.value);return r.set(c,m),m}):b(y,u.value,!0),!0},deleteProperty(d,c){var u=r.get(c);if(u===void 0){if(c in d){const y=l(()=>L(Ot));r.set(c,y),ja(s)}}else b(u,Ot),ja(s);return!0},get(d,c,u){var I;if(c===Dr)return e;var y=r.get(c),m=c in d;if(y===void 0&&(!m||(I=Rr(d,c))!=null&&I.writable)&&(y=l(()=>{var T=Et(m?d[c]:Ot),F=L(T);return F}),r.set(c,y)),y!==void 0){var k=a(y);return k===Ot?void 0:k}return Reflect.get(d,c,u)},getOwnPropertyDescriptor(d,c){var u=Reflect.getOwnPropertyDescriptor(d,c);if(u&&"value"in u){var y=r.get(c);y&&(u.value=a(y))}else if(u===void 0){var m=r.get(c),k=m==null?void 0:m.v;if(m!==void 0&&k!==Ot)return{enumerable:!0,configurable:!0,value:k,writable:!0}}return u},has(d,c){var k;if(c===Dr)return!0;var u=r.get(c),y=u!==void 0&&u.v!==Ot||Reflect.has(d,c);if(u!==void 0||Qe!==null&&(!y||(k=Rr(d,c))!=null&&k.writable)){u===void 0&&(u=l(()=>{var I=y?Et(d[c]):Ot,T=L(I);return T}),r.set(c,u));var m=a(u);if(m===Ot)return!1}return y},set(d,c,u,y){var K;var m=r.get(c),k=c in d;if(n&&c==="length")for(var I=u;I<m.v;I+=1){var T=r.get(I+"");T!==void 0?b(T,Ot):I in d&&(T=l(()=>L(Ot)),r.set(I+"",T))}if(m===void 0)(!k||(K=Rr(d,c))!=null&&K.writable)&&(m=l(()=>L(void 0)),b(m,Et(u)),r.set(c,m));else{k=m.v!==Ot;var F=l(()=>Et(u));b(m,F)}var M=Reflect.getOwnPropertyDescriptor(d,c);if(M!=null&&M.set&&M.set.call(y,u),!k){if(n&&typeof c=="string"){var O=r.get("length"),Z=Number(c);Number.isInteger(Z)&&Z>=O.v&&b(O,Z+1)}ja(s)}return!0},ownKeys(d){a(s);var c=Reflect.ownKeys(d).filter(m=>{var k=r.get(m);return k===void 0||k.v!==Ot});for(var[u,y]of r)y.v!==Ot&&!(u in d)&&c.push(u);return c},setPrototypeOf(){Zo()}})}function ds(e){try{if(e!==null&&typeof e=="object"&&Dr in e)return e[Dr]}catch{}return e}function ji(e,t){return Object.is(ds(e),ds(t))}var cs,Qs,Zs,eo;function Di(){if(cs===void 0){cs=window,Qs=/Firefox/.test(navigator.userAgent);var e=Element.prototype,t=Node.prototype,r=Text.prototype;Zs=Rr(t,"firstChild").get,eo=Rr(t,"nextSibling").get,os(e)&&(e.__click=void 0,e.__className=void 0,e.__attributes=null,e.__style=void 0,e.__e=void 0),os(r)&&(r.__t=void 0)}}function $r(e=""){return document.createTextNode(e)}function Cr(e){return Zs.call(e)}function qa(e){return eo.call(e)}function i(e,t){return Cr(e)}function Ae(e,t=!1){{var r=Cr(e);return r instanceof Comment&&r.data===""?qa(r):r}}function v(e,t=1,r=!1){let n=e;for(;t--;)n=qa(n);return n}function Hi(e){e.textContent=""}function to(){return!1}function Jn(e,t,r){return document.createElementNS(t??Os,e,void 0)}function zi(e,t){if(t){const r=document.body;e.autofocus=!0,_r(()=>{document.activeElement===r&&e.focus()})}}let us=!1;function Ui(){us||(us=!0,document.addEventListener("reset",e=>{Promise.resolve().then(()=>{var t;if(!e.defaultPrevented)for(const r of e.target.elements)(t=r.__on_r)==null||t.call(r)})},{capture:!0}))}function cn(e){var t=He,r=Qe;nr(null),xr(null);try{return e()}finally{nr(t),xr(r)}}function ro(e,t,r,n=r){e.addEventListener(t,()=>cn(r));const s=e.__on_r;s?e.__on_r=()=>{s(),n(!0)}:e.__on_r=()=>n(!0),Ui()}function Bi(e){Qe===null&&(He===null&&Jo(),Ko()),Ur&&Go()}function Wi(e,t){var r=t.last;r===null?t.last=t.first=e:(r.next=e,e.prev=r,t.last=e)}function kr(e,t){var r=Qe;r!==null&&r.f&Ut&&(e|=Ut);var n={ctx:Jt,deps:null,nodes:null,f:e|Rt|rr,first:null,fn:t,last:null,next:null,parent:r,b:r&&r.b,prev:null,teardown:null,wv:0,ac:null},s=n;if(e&Aa)xa!==null?xa.push(n):hr(n);else if(t!==null){try{wa(n)}catch(l){throw jt(n),l}s.deps===null&&s.teardown===null&&s.nodes===null&&s.first===s.last&&!(s.f&$a)&&(s=s.first,e&Br&&e&Mr&&s!==null&&(s.f|=Mr))}if(s!==null&&(s.parent=r,r!==null&&Wi(s,r),He!==null&&He.f&Dt&&!(e&na))){var o=He;(o.effects??(o.effects=[])).push(s)}return n}function Yn(){return He!==null&&!dr}function un(e){const t=kr(ma,null);return At(t,Ft),t.teardown=e,t}function Bt(e){Bi();var t=Qe.f,r=!He&&(t&ur)!==0&&(t&Ea)===0;if(r){var n=Jt;(n.e??(n.e=[])).push(e)}else return ao(e)}function ao(e){return kr(Aa|Bo,e)}function Vi(e){Hr.ensure();const t=kr(na|$a,e);return(r={})=>new Promise(n=>{r.outro?Zr(t,()=>{jt(t),n(void 0)}):(jt(t),n(void 0))})}function fn(e){return kr(Aa,e)}function qi(e){return kr(Vn|$a,e)}function Xn(e,t=0){return kr(ma|t,e)}function N(e,t=[],r=[],n=[]){qs(n,t,r,s=>{kr(ma,()=>e(...s.map(a)))})}function Ca(e,t=0){var r=kr(Br|t,e);return r}function no(e,t=0){var r=kr(on|t,e);return r}function Kt(e){return kr(ur|$a,e)}function so(e){var t=e.teardown;if(t!==null){const r=Ur,n=He;fs(!0),nr(null);try{t.call(null)}finally{fs(r),nr(n)}}}function Qn(e,t=!1){var r=e.first;for(e.first=e.last=null;r!==null;){const s=r.ac;s!==null&&cn(()=>{s.abort(Wr)});var n=r.next;r.f&na?r.parent=null:jt(r,t),r=n}}function Gi(e){for(var t=e.first;t!==null;){var r=t.next;t.f&ur||jt(t),t=r}}function jt(e,t=!0){var r=!1;(t||e.f&Uo)&&e.nodes!==null&&e.nodes.end!==null&&(oo(e.nodes.start,e.nodes.end),r=!0),Qn(e,t&&!r),Ha(e,0),At(e,mr);var n=e.nodes&&e.nodes.t;if(n!==null)for(const o of n)o.stop();so(e);var s=e.parent;s!==null&&s.first!==null&&io(e),e.next=e.prev=e.teardown=e.ctx=e.deps=e.fn=e.nodes=e.ac=null}function oo(e,t){for(;e!==null;){var r=e===t?null:qa(e);e.remove(),e=r}}function io(e){var t=e.parent,r=e.prev,n=e.next;r!==null&&(r.next=n),n!==null&&(n.prev=r),t!==null&&(t.first===e&&(t.first=n),t.last===e&&(t.last=r))}function Zr(e,t,r=!0){var n=[];lo(e,n,!0);var s=()=>{r&&jt(e),t&&t()},o=n.length;if(o>0){var l=()=>--o||s();for(var d of n)d.out(l)}else s()}function lo(e,t,r){if(!(e.f&Ut)){e.f^=Ut;var n=e.nodes&&e.nodes.t;if(n!==null)for(const d of n)(d.is_global||r)&&t.push(d);for(var s=e.first;s!==null;){var o=s.next,l=(s.f&Mr)!==0||(s.f&ur)!==0&&(e.f&Br)!==0;lo(s,t,l?r:!1),s=o}}}function Zn(e){co(e,!0)}function co(e,t){if(e.f&Ut){e.f^=Ut;for(var r=e.first;r!==null;){var n=r.next,s=(r.f&Mr)!==0||(r.f&ur)!==0;co(r,s?t:!1),r=n}var o=e.nodes&&e.nodes.t;if(o!==null)for(const l of o)(l.is_global||t)&&l.in()}}function es(e,t){if(e.nodes)for(var r=e.nodes.start,n=e.nodes.end;r!==null;){var s=r===n?null:qa(r);t.append(r),r=s}}let en=!1,Ur=!1;function fs(e){Ur=e}let He=null,dr=!1;function nr(e){He=e}let Qe=null;function xr(e){Qe=e}let ar=null;function uo(e){He!==null&&(ar===null?ar=[e]:ar.push(e))}let Gt=null,Xt=0,er=null;function Ki(e){er=e}let fo=1,qr=0,ea=qr;function vs(e){ea=e}function vo(){return++fo}function Ga(e){var t=e.f;if(t&Rt)return!0;if(t&Dt&&(e.f&=~ta),t&cr){for(var r=e.deps,n=r.length,s=0;s<n;s++){var o=r[s];if(Ga(o)&&Ks(o),o.wv>e.wv)return!0}t&rr&&Lt===null&&At(e,Ft)}return!1}function go(e,t,r=!0){var n=e.reactions;if(n!==null&&!(ar!==null&&ha.call(ar,e)))for(var s=0;s<n.length;s++){var o=n[s];o.f&Dt?go(o,t,!1):t===o&&(r?At(o,Rt):o.f&Ft&&At(o,cr),hr(o))}}function po(e){var F;var t=Gt,r=Xt,n=er,s=He,o=ar,l=Jt,d=dr,c=ea,u=e.f;Gt=null,Xt=0,er=null,He=u&(ur|na)?null:e,ar=null,_a(e.ctx),dr=!1,ea=++qr,e.ac!==null&&(cn(()=>{e.ac.abort(Wr)}),e.ac=null);try{e.f|=Cn;var y=e.fn,m=y();e.f|=Ea;var k=e.deps,I=Se==null?void 0:Se.is_fork;if(Gt!==null){var T;if(I||Ha(e,Xt),k!==null&&Xt>0)for(k.length=Xt+Gt.length,T=0;T<Gt.length;T++)k[Xt+T]=Gt[T];else e.deps=k=Gt;if(Yn()&&e.f&rr)for(T=Xt;T<k.length;T++)((F=k[T]).reactions??(F.reactions=[])).push(e)}else!I&&k!==null&&Xt<k.length&&(Ha(e,Xt),k.length=Xt);if(Rs()&&er!==null&&!dr&&k!==null&&!(e.f&(Dt|cr|Rt)))for(T=0;T<er.length;T++)go(er[T],e);if(s!==null&&s!==e){if(qr++,s.deps!==null)for(let M=0;M<r;M+=1)s.deps[M].rv=qr;if(t!==null)for(const M of t)M.rv=qr;er!==null&&(n===null?n=er:n.push(...er))}return e.f&jr&&(e.f^=jr),m}catch(M){return Ds(M)}finally{e.f^=Cn,Gt=t,Xt=r,er=n,He=s,ar=o,_a(l),dr=d,ea=c}}function Ji(e,t){let r=t.reactions;if(r!==null){var n=Fo.call(r,e);if(n!==-1){var s=r.length-1;s===0?r=t.reactions=null:(r[n]=r[s],r.pop())}}if(r===null&&t.f&Dt&&(Gt===null||!ha.call(Gt,t))){var o=t;o.f&rr&&(o.f^=rr,o.f&=~ta),Gn(o),Li(o),Ha(o,0)}}function Ha(e,t){var r=e.deps;if(r!==null)for(var n=t;n<r.length;n++)Ji(e,r[n])}function wa(e){var t=e.f;if(!(t&mr)){At(e,Ft);var r=Qe,n=en;Qe=e,en=!0;try{t&(Br|on)?Gi(e):Qn(e),so(e);var s=po(e);e.teardown=typeof s=="function"?s:null,e.wv=fo;var o;En&&yi&&e.f&Rt&&e.deps}finally{en=n,Qe=r}}}async function bo(){await Promise.resolve(),xi()}function a(e){var t=e.f,r=(t&Dt)!==0;if(He!==null&&!dr){var n=Qe!==null&&(Qe.f&mr)!==0;if(!n&&(ar===null||!ha.call(ar,e))){var s=He.deps;if(He.f&Cn)e.rv<qr&&(e.rv=qr,Gt===null&&s!==null&&s[Xt]===e?Xt++:Gt===null?Gt=[e]:Gt.push(e));else{(He.deps??(He.deps=[])).push(e);var o=e.reactions;o===null?e.reactions=[He]:ha.call(o,He)||o.push(He)}}}if(Ur&&zr.has(e))return zr.get(e);if(r){var l=e;if(Ur){var d=l.v;return(!(l.f&Ft)&&l.reactions!==null||ho(l))&&(d=Kn(l)),zr.set(l,d),d}var c=(l.f&rr)===0&&!dr&&He!==null&&(en||(He.f&rr)!==0),u=(l.f&Ea)===0;Ga(l)&&(c&&(l.f|=rr),Ks(l)),c&&!u&&(Js(l),yo(l))}if(Lt!=null&&Lt.has(e))return Lt.get(e);if(e.f&jr)throw e.v;return e.v}function yo(e){if(e.f|=rr,e.deps!==null)for(const t of e.deps)(t.reactions??(t.reactions=[])).push(e),t.f&Dt&&!(t.f&rr)&&(Js(t),yo(t))}function ho(e){if(e.v===Ot)return!0;if(e.deps===null)return!1;for(const t of e.deps)if(zr.has(t)||t.f&Dt&&ho(t))return!0;return!1}function Ma(e){var t=dr;try{return dr=!0,e()}finally{dr=t}}function Yi(e){return e.endsWith("capture")&&e!=="gotpointercapture"&&e!=="lostpointercapture"}const Xi=["beforeinput","click","change","dblclick","contextmenu","focusin","focusout","input","keydown","keyup","mousedown","mousemove","mouseout","mouseover","mouseup","pointerdown","pointermove","pointerout","pointerover","pointerup","touchend","touchmove","touchstart"];function Qi(e){return Xi.includes(e)}const Zi={formnovalidate:"formNoValidate",ismap:"isMap",nomodule:"noModule",playsinline:"playsInline",readonly:"readOnly",defaultvalue:"defaultValue",defaultchecked:"defaultChecked",srcobject:"srcObject",novalidate:"noValidate",allowfullscreen:"allowFullscreen",disablepictureinpicture:"disablePictureInPicture",disableremoteplayback:"disableRemotePlayback"};function el(e){return e=e.toLowerCase(),Zi[e]??e}const tl=["touchstart","touchmove"];function rl(e){return tl.includes(e)}const Gr=Symbol("events"),mo=new Set,Rn=new Set;function _o(e,t,r,n={}){function s(o){if(n.capture||jn.call(t,o),!o.cancelBubble)return cn(()=>r==null?void 0:r.call(this,o))}return e.startsWith("pointer")||e.startsWith("touch")||e==="wheel"?_r(()=>{t.addEventListener(e,s,n)}):t.addEventListener(e,s,n),s}function Er(e,t,r,n,s){var o={capture:n,passive:s},l=_o(e,t,r,o);(t===document.body||t===window||t===document||t instanceof HTMLMediaElement)&&un(()=>{t.removeEventListener(e,l,o)})}function J(e,t,r){(t[Gr]??(t[Gr]={}))[e]=r}function fr(e){for(var t=0;t<e.length;t++)mo.add(e[t]);for(var r of Rn)r(e)}let gs=null;function jn(e){var M,O;var t=this,r=t.ownerDocument,n=e.type,s=((M=e.composedPath)==null?void 0:M.call(e))||[],o=s[0]||e.target;gs=e;var l=0,d=gs===e&&e[Gr];if(d){var c=s.indexOf(d);if(c!==-1&&(t===document||t===window)){e[Gr]=t;return}var u=s.indexOf(t);if(u===-1)return;c<=u&&(l=c)}if(o=s[l]||e.target,o!==t){Ro(e,"currentTarget",{configurable:!0,get(){return o||r}});var y=He,m=Qe;nr(null),xr(null);try{for(var k,I=[];o!==null;){var T=o.assignedSlot||o.parentNode||o.host||null;try{var F=(O=o[Gr])==null?void 0:O[n];F!=null&&(!o.disabled||e.target===o)&&F.call(o,e)}catch(Z){k?I.push(Z):k=Z}if(e.cancelBubble||T===t||T===null)break;o=T}if(k){for(let Z of I)queueMicrotask(()=>{throw Z});throw k}}finally{e[Gr]=t,delete e.currentTarget,nr(y),xr(m)}}}var Es;const hn=((Es=globalThis==null?void 0:globalThis.window)==null?void 0:Es.trustedTypes)&&globalThis.window.trustedTypes.createPolicy("svelte-trusted-html",{createHTML:e=>e});function al(e){return(hn==null?void 0:hn.createHTML(e))??e}function xo(e){var t=Jn("template");return t.innerHTML=al(e.replaceAll("<!>","<!---->")),t.content}function Sa(e,t){var r=Qe;r.nodes===null&&(r.nodes={start:e,end:t,a:null,t:null})}function x(e,t){var r=(t&ci)!==0,n=(t&ui)!==0,s,o=!e.startsWith("<!>");return()=>{s===void 0&&(s=xo(o?e:"<!>"+e),r||(s=Cr(s)));var l=n||Qs?document.importNode(s,!0):s.cloneNode(!0);if(r){var d=Cr(l),c=l.lastChild;Sa(d,c)}else Sa(l,l);return l}}function nl(e,t,r="svg"){var n=!e.startsWith("<!>"),s=`<${r}>${n?e:"<!>"+e}</${r}>`,o;return()=>{if(!o){var l=xo(s),d=Cr(l);o=Cr(d)}var c=o.cloneNode(!0);return Sa(c,c),c}}function sl(e,t){return nl(e,t,"svg")}function Le(){var e=document.createDocumentFragment(),t=document.createComment(""),r=$r();return e.append(t,r),Sa(t,r),e}function f(e,t){e!==null&&e.before(t)}function p(e,t){var r=t==null?"":typeof t=="object"?`${t}`:t;r!==(e.__t??(e.__t=e.nodeValue))&&(e.__t=r,e.nodeValue=`${r}`)}function ol(e,t){return il(e,t)}const Ya=new Map;function il(e,{target:t,anchor:r,props:n={},events:s,context:o,intro:l=!0,transformError:d}){Di();var c=void 0,u=Vi(()=>{var y=r??t.appendChild($r());Ai(y,{pending:()=>{}},I=>{Ce({});var T=Jt;o&&(T.c=o),s&&(n.$$events=s),c=e(I,n)||{},Me()},d);var m=new Set,k=I=>{for(var T=0;T<I.length;T++){var F=I[T];if(!m.has(F)){m.add(F);var M=rl(F);for(const K of[t,document]){var O=Ya.get(K);O===void 0&&(O=new Map,Ya.set(K,O));var Z=O.get(F);Z===void 0?(K.addEventListener(F,jn,{passive:M}),O.set(F,1)):O.set(F,Z+1)}}}};return k(sn(mo)),Rn.add(k),()=>{var M;for(var I of m)for(const O of[t,document]){var T=Ya.get(O),F=T.get(I);--F==0?(O.removeEventListener(I,jn),T.delete(I),T.size===0&&Ya.delete(O)):T.set(I,F)}Rn.delete(k),y!==r&&((M=y.parentNode)==null||M.removeChild(y))}});return ll.set(c,u),c}let ll=new WeakMap;var lr,br,Zt,Qr,Wa,Va,nn;class vn{constructor(t,r=!0){sr(this,"anchor");Ve(this,lr,new Map);Ve(this,br,new Map);Ve(this,Zt,new Map);Ve(this,Qr,new Set);Ve(this,Wa,!0);Ve(this,Va,t=>{if(A(this,lr).has(t)){var r=A(this,lr).get(t),n=A(this,br).get(r);if(n)Zn(n),A(this,Qr).delete(r);else{var s=A(this,Zt).get(r);s&&!(s.effect.f&Ut)&&(A(this,br).set(r,s.effect),A(this,Zt).delete(r),s.fragment.lastChild.remove(),this.anchor.before(s.fragment),n=s.effect)}for(const[o,l]of A(this,lr)){if(A(this,lr).delete(o),o===t)break;const d=A(this,Zt).get(l);d&&(jt(d.effect),A(this,Zt).delete(l))}for(const[o,l]of A(this,br)){if(o===r||A(this,Qr).has(o)||l.f&Ut)continue;const d=()=>{if(Array.from(A(this,lr).values()).includes(o)){var u=document.createDocumentFragment();es(l,u),u.append($r()),A(this,Zt).set(o,{effect:l,fragment:u})}else jt(l);A(this,Qr).delete(o),A(this,br).delete(o)};A(this,Wa)||!n?(A(this,Qr).add(o),Zr(l,d,!1)):d()}}});Ve(this,nn,t=>{A(this,lr).delete(t);const r=Array.from(A(this,lr).values());for(const[n,s]of A(this,Zt))r.includes(n)||(jt(s.effect),A(this,Zt).delete(n))});this.anchor=t,Pe(this,Wa,r)}ensure(t,r){var n=Se,s=to();if(r&&!A(this,br).has(t)&&!A(this,Zt).has(t))if(s){var o=document.createDocumentFragment(),l=$r();o.append(l),A(this,Zt).set(t,{effect:Kt(()=>r(l)),fragment:o})}else A(this,br).set(t,Kt(()=>r(this.anchor)));if(A(this,lr).set(n,t),s){for(const[d,c]of A(this,br))d===t?n.unskip_effect(c):n.skip_effect(c);for(const[d,c]of A(this,Zt))d===t?n.unskip_effect(c.effect):n.skip_effect(c.effect);n.oncommit(A(this,Va)),n.ondiscard(A(this,nn))}else A(this,Va).call(this,n)}}lr=new WeakMap,br=new WeakMap,Zt=new WeakMap,Qr=new WeakMap,Wa=new WeakMap,Va=new WeakMap,nn=new WeakMap;function B(e,t,r=!1){var n=new vn(e),s=r?Mr:0;function o(l,d){n.ensure(l,d)}Ca(()=>{var l=!1;t((d,c=0)=>{l=!0,o(c,d)}),l||o(-1,null)},s)}function dt(e,t){return t}function dl(e,t,r){for(var n=[],s=t.length,o,l=t.length,d=0;d<s;d++){let m=t[d];Zr(m,()=>{if(o){if(o.pending.delete(m),o.done.add(m),o.pending.size===0){var k=e.outrogroups;Dn(e,sn(o.done)),k.delete(o),k.size===0&&(e.outrogroups=null)}}else l-=1},!1)}if(l===0){var c=n.length===0&&r!==null;if(c){var u=r,y=u.parentNode;Hi(y),y.append(u),e.items.clear()}Dn(e,t,!c)}else o={pending:new Set(t),done:new Set},(e.outrogroups??(e.outrogroups=new Set)).add(o)}function Dn(e,t,r=!0){var n;if(e.pending.size>0){n=new Set;for(const l of e.pending.values())for(const d of l)n.add(e.items.get(d).e)}for(var s=0;s<t.length;s++){var o=t[s];if(n!=null&&n.has(o)){o.f|=yr;const l=document.createDocumentFragment();es(o,l)}else jt(t[s],r)}}var ps;function nt(e,t,r,n,s,o=null){var l=e,d=new Map,c=(t&Ps)!==0;if(c){var u=e;l=u.appendChild($r())}var y=null,m=Gs(()=>{var K=r();return Wn(K)?K:K==null?[]:sn(K)}),k,I=new Map,T=!0;function F(K){Z.effect.f&mr||(Z.pending.delete(K),Z.fallback=y,cl(Z,k,l,t,n),y!==null&&(k.length===0?y.f&yr?(y.f^=yr,Fa(y,null,l)):Zn(y):Zr(y,()=>{y=null})))}function M(K){Z.pending.delete(K)}var O=Ca(()=>{k=a(m);for(var K=k.length,E=new Set,g=Se,$=to(),C=0;C<K;C+=1){var D=k[C],ue=n(D,C),pe=T?null:d.get(ue);pe?(pe.v&&ka(pe.v,D),pe.i&&ka(pe.i,C),$&&g.unskip_effect(pe.e)):(pe=ul(d,T?l:ps??(ps=$r()),D,ue,C,s,t,r),T||(pe.e.f|=yr),d.set(ue,pe)),E.add(ue)}if(K===0&&o&&!y&&(T?y=Kt(()=>o(l)):(y=Kt(()=>o(ps??(ps=$r()))),y.f|=yr)),K>E.size&&qo(),!T)if(I.set(g,E),$){for(const[Ke,ze]of d)E.has(Ke)||g.skip_effect(ze.e);g.oncommit(F),g.ondiscard(M)}else F(g);a(m)}),Z={effect:O,items:d,pending:I,outrogroups:null,fallback:y};T=!1}function Pa(e){for(;e!==null&&!(e.f&ur);)e=e.next;return e}function cl(e,t,r,n,s){var pe,Ke,ze,W,Q,ve,se,Oe,z;var o=(n&ni)!==0,l=t.length,d=e.items,c=Pa(e.effect.first),u,y=null,m,k=[],I=[],T,F,M,O;if(o)for(O=0;O<l;O+=1)T=t[O],F=s(T,O),M=d.get(F).e,M.f&yr||((Ke=(pe=M.nodes)==null?void 0:pe.a)==null||Ke.measure(),(m??(m=new Set)).add(M));for(O=0;O<l;O+=1){if(T=t[O],F=s(T,O),M=d.get(F).e,e.outrogroups!==null)for(const V of e.outrogroups)V.pending.delete(M),V.done.delete(M);if(M.f&yr)if(M.f^=yr,M===c)Fa(M,null,r);else{var Z=y?y.next:c;M===e.effect.last&&(e.effect.last=M.prev),M.prev&&(M.prev.next=M.next),M.next&&(M.next.prev=M.prev),Tr(e,y,M),Tr(e,M,Z),Fa(M,Z,r),y=M,k=[],I=[],c=Pa(y.next);continue}if(M.f&Ut&&(Zn(M),o&&((W=(ze=M.nodes)==null?void 0:ze.a)==null||W.unfix(),(m??(m=new Set)).delete(M))),M!==c){if(u!==void 0&&u.has(M)){if(k.length<I.length){var K=I[0],E;y=K.prev;var g=k[0],$=k[k.length-1];for(E=0;E<k.length;E+=1)Fa(k[E],K,r);for(E=0;E<I.length;E+=1)u.delete(I[E]);Tr(e,g.prev,$.next),Tr(e,y,g),Tr(e,$,K),c=K,y=$,O-=1,k=[],I=[]}else u.delete(M),Fa(M,c,r),Tr(e,M.prev,M.next),Tr(e,M,y===null?e.effect.first:y.next),Tr(e,y,M),y=M;continue}for(k=[],I=[];c!==null&&c!==M;)(u??(u=new Set)).add(c),I.push(c),c=Pa(c.next);if(c===null)continue}M.f&yr||k.push(M),y=M,c=Pa(M.next)}if(e.outrogroups!==null){for(const V of e.outrogroups)V.pending.size===0&&(Dn(e,sn(V.done)),(Q=e.outrogroups)==null||Q.delete(V));e.outrogroups.size===0&&(e.outrogroups=null)}if(c!==null||u!==void 0){var C=[];if(u!==void 0)for(M of u)M.f&Ut||C.push(M);for(;c!==null;)!(c.f&Ut)&&c!==e.fallback&&C.push(c),c=Pa(c.next);var D=C.length;if(D>0){var ue=n&Ps&&l===0?r:null;if(o){for(O=0;O<D;O+=1)(se=(ve=C[O].nodes)==null?void 0:ve.a)==null||se.measure();for(O=0;O<D;O+=1)(z=(Oe=C[O].nodes)==null?void 0:Oe.a)==null||z.fix()}dl(e,C,ue)}}o&&_r(()=>{var V,me;if(m!==void 0)for(M of m)(me=(V=M.nodes)==null?void 0:V.a)==null||me.apply()})}function ul(e,t,r,n,s,o,l,d){var c=l&ri?l&si?ra(r):Fi(r,!1,!1):null,u=l&ai?ra(s):null;return{v:c,i:u,e:Kt(()=>(o(t,c??r,u??s,d),()=>{e.delete(n)}))}}function Fa(e,t,r){if(e.nodes)for(var n=e.nodes.start,s=e.nodes.end,o=t&&!(t.f&yr)?t.nodes.start:r;n!==null;){var l=qa(n);if(o.before(n),n===s)return;n=l}}function Tr(e,t,r){t===null?e.effect.first=r:t.next=r,r===null?e.effect.last=t:r.prev=t}function fl(e,t,r=!1,n=!1,s=!1){var o=e,l="";N(()=>{var d=Qe;if(l!==(l=t()??"")&&(d.nodes!==null&&(oo(d.nodes.start,d.nodes.end),d.nodes=null),l!=="")){var c=r?Is:n?fi:void 0,u=Jn(r?"svg":n?"math":"template",c);u.innerHTML=l;var y=r||n?u:u.content;if(Sa(Cr(y),y.lastChild),r||n)for(;Cr(y);)o.before(Cr(y));else o.before(y)}})}function bt(e,t,...r){var n=new vn(e);Ca(()=>{const s=t()??null;n.ensure(s,s&&(o=>s(o,...r)))},Mr)}function vl(e,t,r){var n=new vn(e);Ca(()=>{var s=t()??null;n.ensure(s,s&&(o=>r(o,s)))},Mr)}function gl(e,t,r,n,s,o){var l=null,d=e,c=new vn(d,!1);Ca(()=>{const u=t()||null;var y=Is;if(u===null){c.ensure(null,null);return}return c.ensure(u,m=>{if(u){if(l=Jn(u,y),Sa(l,l),n){var k=l.appendChild($r());n(l,k)}Qe.nodes.end=l,m.before(l)}}),()=>{}},Mr),un(()=>{})}function pl(e,t){var r=void 0,n;no(()=>{r!==(r=t())&&(n&&(jt(n),n=null),r&&(n=Kt(()=>{fn(()=>r(e))})))})}function ko(e){var t,r,n="";if(typeof e=="string"||typeof e=="number")n+=e;else if(typeof e=="object")if(Array.isArray(e)){var s=e.length;for(t=0;t<s;t++)e[t]&&(r=ko(e[t]))&&(n&&(n+=" "),n+=r)}else for(r in e)e[r]&&(n&&(n+=" "),n+=r);return n}function bl(){for(var e,t,r=0,n="",s=arguments.length;r<s;r++)(e=arguments[r])&&(t=ko(e))&&(n&&(n+=" "),n+=t);return n}function wo(e){return typeof e=="object"?bl(e):e??""}const bs=[...` 	
\r\f \v\uFEFF`];function yl(e,t,r){var n=e==null?"":""+e;if(r){for(var s of Object.keys(r))if(r[s])n=n?n+" "+s:s;else if(n.length)for(var o=s.length,l=0;(l=n.indexOf(s,l))>=0;){var d=l+o;(l===0||bs.includes(n[l-1]))&&(d===n.length||bs.includes(n[d]))?n=(l===0?"":n.substring(0,l))+n.substring(d+1):l=d}}return n===""?null:n}function ys(e,t=!1){var r=t?" !important;":";",n="";for(var s of Object.keys(e)){var o=e[s];o!=null&&o!==""&&(n+=" "+s+": "+o+r)}return n}function mn(e){return e[0]!=="-"||e[1]!=="-"?e.toLowerCase():e}function hl(e,t){if(t){var r="",n,s;if(Array.isArray(t)?(n=t[0],s=t[1]):n=t,e){e=String(e).replaceAll(/\s*\/\*.*?\*\/\s*/g,"").trim();var o=!1,l=0,d=!1,c=[];n&&c.push(...Object.keys(n).map(mn)),s&&c.push(...Object.keys(s).map(mn));var u=0,y=-1;const F=e.length;for(var m=0;m<F;m++){var k=e[m];if(d?k==="/"&&e[m-1]==="*"&&(d=!1):o?o===k&&(o=!1):k==="/"&&e[m+1]==="*"?d=!0:k==='"'||k==="'"?o=k:k==="("?l++:k===")"&&l--,!d&&o===!1&&l===0){if(k===":"&&y===-1)y=m;else if(k===";"||m===F-1){if(y!==-1){var I=mn(e.substring(u,y).trim());if(!c.includes(I)){k!==";"&&m++;var T=e.substring(u,m).trim();r+=" "+T+";"}}u=m+1,y=-1}}}}return n&&(r+=ys(n)),s&&(r+=ys(s,!0)),r=r.trim(),r===""?null:r}return e==null?null:String(e)}function et(e,t,r,n,s,o){var l=e.__className;if(l!==r||l===void 0){var d=yl(r,n,o);d==null?e.removeAttribute("class"):t?e.className=d:e.setAttribute("class",d),e.__className=r}else if(o&&s!==o)for(var c in o){var u=!!o[c];(s==null||u!==!!s[c])&&e.classList.toggle(c,u)}return o}function _n(e,t={},r,n){for(var s in r){var o=r[s];t[s]!==o&&(r[s]==null?e.style.removeProperty(s):e.style.setProperty(s,o,n))}}function ml(e,t,r,n){var s=e.__style;if(s!==t){var o=hl(t,n);o==null?e.removeAttribute("style"):e.style.cssText=o,e.__style=t}else n&&(Array.isArray(n)?(_n(e,r==null?void 0:r[0],n[0]),_n(e,r==null?void 0:r[1],n[1],"important")):_n(e,r,n));return n}function za(e,t,r=!1){if(e.multiple){if(t==null)return;if(!Wn(t))return gi();for(var n of e.options)n.selected=t.includes(Da(n));return}for(n of e.options){var s=Da(n);if(ji(s,t)){n.selected=!0;return}}(!r||t!==void 0)&&(e.selectedIndex=-1)}function ts(e){var t=new MutationObserver(()=>{za(e,e.__value)});t.observe(e,{childList:!0,subtree:!0,attributes:!0,attributeFilter:["value"]}),un(()=>{t.disconnect()})}function Hn(e,t,r=t){var n=new WeakSet,s=!0;ro(e,"change",o=>{var l=o?"[selected]":":checked",d;if(e.multiple)d=[].map.call(e.querySelectorAll(l),Da);else{var c=e.querySelector(l)??e.querySelector("option:not([disabled])");d=c&&Da(c)}r(d),Se!==null&&n.add(Se)}),fn(()=>{var o=t();if(e===document.activeElement){var l=tn??Se;if(n.has(l))return}if(za(e,o,s),s&&o===void 0){var d=e.querySelector(":checked");d!==null&&(o=Da(d),r(o))}e.__value=o,s=!1}),ts(e)}function Da(e){return"__value"in e?e.__value:e.value}const Oa=Symbol("class"),Ia=Symbol("style"),So=Symbol("is custom element"),Ao=Symbol("is html"),_l=qn?"option":"OPTION",xl=qn?"select":"SELECT",kl=qn?"progress":"PROGRESS";function wr(e,t){var r=rs(e);r.value===(r.value=t??void 0)||e.value===t&&(t!==0||e.nodeName!==kl)||(e.value=t??"")}function wl(e,t){t?e.hasAttribute("selected")||e.setAttribute("selected",""):e.removeAttribute("selected")}function pt(e,t,r,n){var s=rs(e);s[t]!==(s[t]=r)&&(t==="loading"&&(e[Wo]=r),r==null?e.removeAttribute(t):typeof r!="string"&&Eo(e).includes(t)?e[t]=r:e.setAttribute(t,r))}function Sl(e,t,r,n,s=!1,o=!1){var l=rs(e),d=l[So],c=!l[Ao],u=t||{},y=e.nodeName===_l;for(var m in t)m in r||(r[m]=null);r.class?r.class=wo(r.class):r[Oa]&&(r.class=null),r[Ia]&&(r.style??(r.style=null));var k=Eo(e);for(const E in r){let g=r[E];if(y&&E==="value"&&g==null){e.value=e.__value="",u[E]=g;continue}if(E==="class"){var I=e.namespaceURI==="http://www.w3.org/1999/xhtml";et(e,I,g,n,t==null?void 0:t[Oa],r[Oa]),u[E]=g,u[Oa]=r[Oa];continue}if(E==="style"){ml(e,g,t==null?void 0:t[Ia],r[Ia]),u[E]=g,u[Ia]=r[Ia];continue}var T=u[E];if(!(g===T&&!(g===void 0&&e.hasAttribute(E)))){u[E]=g;var F=E[0]+E[1];if(F!=="$$")if(F==="on"){const $={},C="$$"+E;let D=E.slice(2);var M=Qi(D);if(Yi(D)&&(D=D.slice(0,-7),$.capture=!0),!M&&T){if(g!=null)continue;e.removeEventListener(D,u[C],$),u[C]=null}if(M)J(D,e,g),fr([D]);else if(g!=null){let ue=function(pe){u[E].call(this,pe)};var K=ue;u[C]=_o(D,e,ue,$)}}else if(E==="style")pt(e,E,g);else if(E==="autofocus")zi(e,!!g);else if(!d&&(E==="__value"||E==="value"&&g!=null))e.value=e.__value=g;else if(E==="selected"&&y)wl(e,g);else{var O=E;c||(O=el(O));var Z=O==="defaultValue"||O==="defaultChecked";if(g==null&&!d&&!Z)if(l[E]=null,O==="value"||O==="checked"){let $=e;const C=t===void 0;if(O==="value"){let D=$.defaultValue;$.removeAttribute(O),$.defaultValue=D,$.value=$.__value=C?D:null}else{let D=$.defaultChecked;$.removeAttribute(O),$.defaultChecked=D,$.checked=C?D:!1}}else e.removeAttribute(E);else Z||k.includes(O)&&(d||typeof g!="string")?(e[O]=g,O in l&&(l[O]=Ot)):typeof g!="function"&&pt(e,O,g)}}}return u}function hs(e,t,r=[],n=[],s=[],o,l=!1,d=!1){qs(s,r,n,c=>{var u=void 0,y={},m=e.nodeName===xl,k=!1;if(no(()=>{var T=t(...c.map(a)),F=Sl(e,u,T,o,l,d);k&&m&&"value"in T&&za(e,T.value);for(let O of Object.getOwnPropertySymbols(y))T[O]||jt(y[O]);for(let O of Object.getOwnPropertySymbols(T)){var M=T[O];O.description===vi&&(!u||M!==u[O])&&(y[O]&&jt(y[O]),y[O]=Kt(()=>pl(e,()=>M))),F[O]=M}u=F}),m){var I=e;fn(()=>{za(I,u.value,!0),ts(I)})}k=!0})}function rs(e){return e.__attributes??(e.__attributes={[So]:e.nodeName.includes("-"),[Ao]:e.namespaceURI===Os})}var ms=new Map;function Eo(e){var t=e.getAttribute("is")||e.nodeName,r=ms.get(t);if(r)return r;ms.set(t,r=[]);for(var n,s=e,o=Element.prototype;o!==s;){n=jo(s);for(var l in n)n[l].set&&r.push(l);s=Cs(s)}return r}function Kr(e,t,r=t){var n=new WeakSet;ro(e,"input",async s=>{var o=s?e.defaultValue:e.value;if(o=xn(e)?kn(o):o,r(o),Se!==null&&n.add(Se),await bo(),o!==(o=t())){var l=e.selectionStart,d=e.selectionEnd,c=e.value.length;if(e.value=o??"",d!==null){var u=e.value.length;l===d&&d===c&&u>c?(e.selectionStart=u,e.selectionEnd=u):(e.selectionStart=l,e.selectionEnd=Math.min(d,u))}}}),Ma(t)==null&&e.value&&(r(xn(e)?kn(e.value):e.value),Se!==null&&n.add(Se)),Xn(()=>{var s=t();if(e===document.activeElement){var o=tn??Se;if(n.has(o))return}xn(e)&&s===kn(e.value)||e.type==="date"&&!s&&!e.value||s!==e.value&&(e.value=s??"")})}function xn(e){var t=e.type;return t==="number"||t==="range"}function kn(e){return e===""?null:+e}function _s(e,t){return e===t||(e==null?void 0:e[Dr])===t}function zn(e={},t,r,n){return fn(()=>{var s,o;return Xn(()=>{s=o,o=[],Ma(()=>{e!==r(...o)&&(t(e,...o),s&&_s(r(...s),e)&&t(null,...s))})}),()=>{_r(()=>{o&&_s(r(...o),e)&&t(null,...o)})}}),e}let Xa=!1;function Al(e){var t=Xa;try{return Xa=!1,[e(),Xa]}finally{Xa=t}}const El={get(e,t){if(!e.exclude.includes(t))return e.props[t]},set(e,t){return!1},getOwnPropertyDescriptor(e,t){if(!e.exclude.includes(t)&&t in e.props)return{enumerable:!0,configurable:!0,value:e.props[t]}},has(e,t){return e.exclude.includes(t)?!1:t in e.props},ownKeys(e){return Reflect.ownKeys(e.props).filter(t=>!e.exclude.includes(t))}};function yt(e,t,r){return new Proxy({props:e,exclude:t},El)}const $l={get(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ta(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n)return n[t]}},set(e,t,r){let n=e.props.length;for(;n--;){let s=e.props[n];Ta(s)&&(s=s());const o=Rr(s,t);if(o&&o.set)return o.set(r),!0}return!1},getOwnPropertyDescriptor(e,t){let r=e.props.length;for(;r--;){let n=e.props[r];if(Ta(n)&&(n=n()),typeof n=="object"&&n!==null&&t in n){const s=Rr(n,t);return s&&!s.configurable&&(s.configurable=!0),s}}},has(e,t){if(t===Dr||t===Ns)return!1;for(let r of e.props)if(Ta(r)&&(r=r()),r!=null&&t in r)return!0;return!1},ownKeys(e){const t=[];for(let r of e.props)if(Ta(r)&&(r=r()),!!r){for(const n in r)t.includes(n)||t.push(n);for(const n of Object.getOwnPropertySymbols(r))t.includes(n)||t.push(n)}return t}};function mt(...e){return new Proxy({props:e},$l)}function la(e,t,r,n){var Z;var s=(r&li)!==0,o=(r&di)!==0,l=n,d=!0,c=()=>(d&&(d=!1,l=o?Ma(n):n),l),u;if(s){var y=Dr in e||Ns in e;u=((Z=Rr(e,t))==null?void 0:Z.set)??(y&&t in e?K=>e[t]=K:void 0)}var m,k=!1;s?[m,k]=Al(()=>e[t]):m=e[t],m===void 0&&n!==void 0&&(m=c(),u&&(Xo(),u(m)));var I;if(I=()=>{var K=e[t];return K===void 0?c():(d=!0,K)},!(r&ii))return I;if(u){var T=e.$$legacy;return function(K,E){return arguments.length>0?((!E||T||k)&&u(E?I():K),K):I()}}var F=!1,M=(r&oi?dn:Gs)(()=>(F=!1,I()));s&&a(M);var O=Qe;return function(K,E){if(arguments.length>0){const g=E?a(M):s?Et(K):K;return b(M,g),F=!0,l!==void 0&&(l=g),K}return Ur&&F||O.f&mr?M.v:a(M)}}function Cl(e){Jt===null&&Ts(),Bt(()=>{const t=Ma(e);if(typeof t=="function")return t})}function Ml(e){Jt===null&&Ts(),Cl(()=>()=>Ma(e))}const Nl="5";var $s;typeof window<"u"&&(($s=window.__svelte??(window.__svelte={})).v??($s.v=new Set)).add(Nl);const as="prx-console-token",Tl=[{labelKey:"nav.overview",path:"/overview"},{labelKey:"nav.sessions",path:"/sessions"},{labelKey:"nav.channels",path:"/channels"},{labelKey:"nav.hooks",path:"/hooks"},{labelKey:"nav.mcp",path:"/mcp"},{labelKey:"nav.skills",path:"/skills"},{labelKey:"nav.plugins",path:"/plugins"},{labelKey:"nav.config",path:"/config"},{labelKey:"nav.logs",path:"/logs"}];function Ua(){var e;return typeof window>"u"?"":((e=window.localStorage.getItem(as))==null?void 0:e.trim())??""}function Pl(e){typeof window>"u"||window.localStorage.setItem(as,e.trim())}function $o(){typeof window>"u"||window.localStorage.removeItem(as)}const Ol={title:"PRX Console",menu:"Menu",closeSidebar:"Close sidebar",language:"Language",notFound:"Not found",backToOverview:"Back to Overview"},Il={overview:"Overview",sessions:"Sessions",channels:"Channels",config:"Config",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"Plugins",logs:"Logs"},Ll={logout:"Logout",loading:"Loading...",error:"Error",refresh:"Refresh",updatedAt:"Updated {time}",na:"N/A",enabled:"Enabled",disabled:"Disabled",yes:"Yes",no:"No",unknown:"Unknown",clipboardUnavailable:"Clipboard not available.",copied:"Copied",copyFailed:"Copy failed",empty:"Empty"},Fl={title:"Overview",version:"Version",uptime:"Uptime",model:"Model",memoryBackend:"Memory Backend",gatewayPort:"Gateway Port",configuredChannels:"Configured Channels",loading:"Loading status...",loadFailed:"Failed to load status.",noChannelsConfigured:"No channels configured."},Rl={title:"Sessions",sessionId:"Session ID",sender:"Sender",channel:"Channel",messages:"Messages",lastMessage:"Last Message",loading:"Loading sessions...",loadFailed:"Failed to load sessions.",none:"No active sessions"},jl={title:"Chat",session:"Session",back:"Back to Sessions",loading:"Loading messages...",loadFailed:"Failed to load messages.",sendFailed:"Failed to send message.",empty:"No messages in this session.",inputPlaceholder:"Type a message...",send:"Send",sending:"Sending..."},Dl={title:"Channels",type:"Type",loading:"Loading channels...",loadFailed:"Failed to load channel status.",noChannels:"No channels available.",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"Email",irc:"IRC",lark:"Lark",dingtalk:"DingTalk",qq:"QQ",cli:"CLI"}},Hl={title:"Config",rawJson:"Raw JSON",structured:"Structured View",copy:"Copy",copyJson:"Copy JSON",loading:"Loading config...",loadFailed:"Failed to load config.",section:{general:"General",gateway:"Gateway",channels:"Channels",memory:"Memory",security:"Security",model:"Model",other:"Other"},field:{version:"Version",runtimeModel:"Runtime Model",memoryBackend:"Memory Backend",configuredChannels:"Configured Channels",notConfigured:"Not configured",notSet:"Not set"},channel:{settings:"settings",notConfigured:"Not configured"},redacted:"Redacted",emptyObject:"No settings"},zl={title:"Logs",connected:"Connected",disconnected:"Disconnected",reconnecting:"Reconnecting",pause:"Pause",resume:"Resume",clear:"Clear",waiting:"Waiting for log stream..."},Ul={title:"Hooks",loading:"Loading hooks...",noHooks:"No hooks configured.",addHook:"Add Hook",cancelAdd:"Cancel",newHook:"New Hook",event:"Event",command:"Command",commandPlaceholder:"e.g. /opt/scripts/on-event.sh",timeout:"Timeout (ms)",enabled:"Enabled",edit:"Edit",delete:"Delete",save:"Save",cancel:"Cancel"},Bl={title:"MCP Servers",loading:"Loading MCP servers...",noServers:"No MCP servers configured.",connected:"Connected",disconnected:"Disconnected",tools:"tools",availableTools:"Available Tools",noTools:"No tools available."},Wl={title:"Skills",loading:"Loading skills...",noSkills:"No skills installed.",active:"active",tabInstalled:"Installed",tabDiscover:"Discover",search:"Search skills...",source:"Source",searchBtn:"Search",searching:"Searching...",noResults:"No results found.",install:"Install",installing:"Installing...",installed:"Installed",uninstall:"Uninstall",uninstalling:"Removing...",confirmUninstall:'Are you sure you want to uninstall "{name}"?',stars:"stars",owner:"by",licensed:"Licensed",unlicensed:"No license",installSuccess:"Skill installed successfully",installFailed:"Failed to install skill",uninstallSuccess:"Skill uninstalled",uninstallFailed:"Failed to uninstall skill"},Vl={title:"Plugins",loading:"Loading plugins...",loadFailed:"Failed to load plugins.",noPlugins:"No WASM plugins loaded.",capabilities:"Capabilities",permissions:"Permissions",statusActive:"Active",reload:"Reload",reloadSuccess:'Plugin "{name}" reloaded',reloadFailed:"Failed to reload plugin"},ql={title:"PRX Console Login",accessToken:"Access Token",login:"Login",hint:"Enter your gateway auth token to continue.",placeholder:"Bearer token",tokenRequired:"Access token is required."},Gl={app:Ol,nav:Il,common:Ll,overview:Fl,sessions:Rl,chat:jl,channels:Dl,config:Hl,logs:zl,hooks:Ul,mcp:Bl,skills:Wl,plugins:Vl,login:ql},Kl={title:"PRX 控制台",menu:"菜单",closeSidebar:"关闭侧边栏",language:"语言",notFound:"页面未找到",backToOverview:"返回概览"},Jl={overview:"概览",sessions:"会话",channels:"通道",config:"配置",hooks:"Hooks",mcp:"MCP",skills:"Skills",plugins:"插件",logs:"日志"},Yl={logout:"退出登录",loading:"加载中...",error:"错误",refresh:"刷新",updatedAt:"更新时间 {time}",na:"暂无",enabled:"已启用",disabled:"已禁用",yes:"是",no:"否",unknown:"未知",clipboardUnavailable:"当前环境不支持剪贴板。",copied:"已复制",copyFailed:"复制失败",empty:"空"},Xl={title:"概览",version:"版本",uptime:"运行时长",model:"模型",memoryBackend:"记忆后端",gatewayPort:"网关端口",configuredChannels:"已配置通道",loading:"正在加载状态...",loadFailed:"加载状态失败。",noChannelsConfigured:"尚未配置任何通道。"},Ql={title:"会话",sessionId:"会话 ID",sender:"发送方",channel:"通道",messages:"消息数",lastMessage:"最后消息",loading:"正在加载会话...",loadFailed:"加载会话失败。",none:"当前没有活跃会话"},Zl={title:"聊天",session:"会话",back:"返回会话列表",loading:"正在加载消息...",loadFailed:"加载消息失败。",sendFailed:"发送消息失败。",empty:"此会话暂无消息。",inputPlaceholder:"输入消息...",send:"发送",sending:"发送中..."},ed={title:"通道",type:"类型",loading:"正在加载通道状态...",loadFailed:"加载通道状态失败。",noChannels:"暂无通道数据。",names:{signal:"Signal",whatsapp:"WhatsApp",linq:"LINQ",nextcloud_talk:"Nextcloud Talk",telegram:"Telegram",discord:"Discord",slack:"Slack",mattermost:"Mattermost",webhook:"Webhook",imessage:"iMessage",matrix:"Matrix",wacli:"WA CLI",email:"邮件",irc:"IRC",lark:"飞书",dingtalk:"钉钉",qq:"QQ",cli:"命令行"}},td={title:"配置",rawJson:"原始 JSON",structured:"结构化视图",copy:"复制",copyJson:"复制 JSON",loading:"正在加载配置...",loadFailed:"加载配置失败。",section:{general:"常规",gateway:"网关",channels:"通道",memory:"记忆",security:"安全",model:"模型",other:"其他"},field:{version:"版本",runtimeModel:"运行模型",memoryBackend:"记忆后端",configuredChannels:"已配置通道",notConfigured:"未配置",notSet:"未设置"},channel:{settings:"配置",notConfigured:"未配置"},redacted:"已脱敏",emptyObject:"无配置项"},rd={title:"日志",connected:"已连接",disconnected:"已断开",reconnecting:"重连中",pause:"暂停",resume:"继续",clear:"清空",waiting:"等待日志流..."},ad={title:"Hooks",loading:"正在加载 Hooks...",noHooks:"尚未配置任何 Hook。",addHook:"添加 Hook",cancelAdd:"取消",newHook:"新建 Hook",event:"事件",command:"命令",commandPlaceholder:"例如 /opt/scripts/on-event.sh",timeout:"超时 (ms)",enabled:"启用",edit:"编辑",delete:"删除",save:"保存",cancel:"取消"},nd={title:"MCP 服务",loading:"正在加载 MCP 服务...",noServers:"尚未配置任何 MCP 服务。",connected:"已连接",disconnected:"已断开",tools:"个工具",availableTools:"可用工具",noTools:"无可用工具。"},sd={title:"Skills",loading:"正在加载 Skills...",noSkills:"尚未安装任何 Skill。",active:"已启用",tabInstalled:"已安装",tabDiscover:"发现新 Skills",search:"搜索 Skills...",source:"来源",searchBtn:"搜索",searching:"搜索中...",noResults:"未找到结果。",install:"安装",installing:"安装中...",installed:"已安装",uninstall:"卸载",uninstalling:"卸载中...",confirmUninstall:'确定要卸载 "{name}" 吗？',stars:"星标",owner:"作者",licensed:"有许可证",unlicensed:"无许可证",installSuccess:"Skill 安装成功",installFailed:"Skill 安装失败",uninstallSuccess:"Skill 已卸载",uninstallFailed:"Skill 卸载失败"},od={title:"插件",loading:"正在加载插件...",loadFailed:"加载插件失败。",noPlugins:"未加载任何 WASM 插件。",capabilities:"能力",permissions:"权限",statusActive:"运行中",reload:"重载",reloadSuccess:'插件 "{name}" 已重载',reloadFailed:"插件重载失败"},id={title:"PRX 控制台登录",accessToken:"访问令牌",login:"登录",hint:"请输入网关认证令牌以继续。",placeholder:"Bearer 令牌",tokenRequired:"访问令牌不能为空。"},ld={app:Kl,nav:Jl,common:Yl,overview:Xl,sessions:Ql,chat:Zl,channels:ed,config:td,logs:rd,hooks:ad,mcp:nd,skills:sd,plugins:od,login:id},gn="prx-console-lang",Ba="en",wn={en:Gl,zh:ld};function Un(e){return typeof e!="string"||e.trim().length===0?Ba:e.trim().toLowerCase().startsWith("zh")?"zh":"en"}function dd(){var e;if(typeof window<"u"){const t=window.localStorage.getItem(gn);if(t)return Un(t)}if(typeof navigator<"u"){const t=navigator.language||((e=navigator.languages)==null?void 0:e[0])||Ba;return Un(t)}return Ba}function xs(e,t){return t.split(".").reduce((r,n)=>{if(!(!r||typeof r!="object"))return r[n]},e)}function Co(e){typeof document<"u"&&(document.documentElement.lang=e==="zh"?"zh-CN":"en")}function cd(e){typeof window<"u"&&window.localStorage.setItem(gn,e)}const aa=Et({lang:dd()});Co(aa.lang);function Mo(e){const t=Un(e);aa.lang!==t&&(aa.lang=t,cd(t),Co(t))}function da(){Mo(aa.lang==="en"?"zh":"en")}function ud(){if(typeof window>"u")return;const e=window.localStorage.getItem(gn);e&&Mo(e)}function h(e,t={}){const r=wn[aa.lang]??wn[Ba];let n=xs(r,e);if(typeof n!="string"&&(n=xs(wn[Ba],e)),typeof n!="string")return e;for(const[s,o]of Object.entries(t))n=n.replaceAll(`{${s}}`,String(o));return n}function No(){return typeof window>"u"?"/":window.location.pathname||"/"}function Pr(e,t=!1){if(typeof window>"u")return;e.startsWith("/")||(e=`/${e}`);const r=t?"replaceState":"pushState";window.location.pathname!==e&&(window.history[r]({},"",e),window.dispatchEvent(new PopStateEvent("popstate")))}function fd(e){if(typeof window>"u")return()=>{};const t=()=>{e(No())};return window.addEventListener("popstate",t),t(),()=>{window.removeEventListener("popstate",t)}}/**
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
 */const vd={xmlns:"http://www.w3.org/2000/svg",width:24,height:24,viewBox:"0 0 24 24",fill:"none",stroke:"currentColor","stroke-width":2,"stroke-linecap":"round","stroke-linejoin":"round"};/**
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
 */const gd=e=>{for(const t in e)if(t.startsWith("aria-")||t==="role"||t==="title")return!0;return!1};var pd=sl("<svg><!><!></svg>");function _t(e,t){Ce(t,!0);const r=la(t,"color",3,"currentColor"),n=la(t,"size",3,24),s=la(t,"strokeWidth",3,2),o=la(t,"absoluteStrokeWidth",3,!1),l=la(t,"iconNode",19,()=>[]),d=yt(t,["$$slots","$$events","$$legacy","name","color","size","strokeWidth","absoluteStrokeWidth","iconNode","children"]);var c=pd();hs(c,(m,k)=>({...vd,...m,...d,width:n(),height:n(),stroke:r(),"stroke-width":k,class:["lucide-icon lucide",t.name&&`lucide-${t.name}`,t.class]}),[()=>!t.children&&!gd(d)&&{"aria-hidden":"true"},()=>o()?Number(s())*24/Number(n()):s()]);var u=i(c);nt(u,17,l,dt,(m,k)=>{var I=ne(()=>La(a(k),2));let T=()=>a(I)[0],F=()=>a(I)[1];var M=Le(),O=Ae(M);gl(O,T,!0,(Z,K)=>{hs(Z,()=>({...F()}))}),f(m,M)});var y=v(u);bt(y,()=>t.children??we),f(e,c),Me()}function bd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3.85 8.62a4 4 0 0 1 4.78-4.77 4 4 0 0 1 6.74 0 4 4 0 0 1 4.78 4.78 4 4 0 0 1 0 6.74 4 4 0 0 1-4.77 4.78 4 4 0 0 1-6.75 0 4 4 0 0 1-4.78-4.77 4 4 0 0 1 0-6.76Z"}],["path",{d:"m9 12 2 2 4-4"}]];_t(e,mt({name:"badge-check"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function ks(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M10 22V7a1 1 0 0 0-1-1H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5a1 1 0 0 0-1-1H2"}],["rect",{x:"14",y:"2",width:"8",height:"8",rx:"1"}]];_t(e,mt({name:"blocks"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function yd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 8V4H8"}],["rect",{width:"16",height:"12",x:"4",y:"8",rx:"2"}],["path",{d:"M2 14h2"}],["path",{d:"M20 14h2"}],["path",{d:"M15 13v2"}],["path",{d:"M9 13v2"}]];_t(e,mt({name:"bot"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function hd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 18V5"}],["path",{d:"M15 13a4.17 4.17 0 0 1-3-4 4.17 4.17 0 0 1-3 4"}],["path",{d:"M17.598 6.5A3 3 0 1 0 12 5a3 3 0 1 0-5.598 1.5"}],["path",{d:"M17.997 5.125a4 4 0 0 1 2.526 5.77"}],["path",{d:"M18 18a4 4 0 0 0 2-7.464"}],["path",{d:"M19.967 17.483A4 4 0 1 1 12 18a4 4 0 1 1-7.967-.517"}],["path",{d:"M6 18a4 4 0 0 1-2-7.464"}],["path",{d:"M6.003 5.125a4 4 0 0 0-2.526 5.77"}]];_t(e,mt({name:"brain"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function md(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M17 19a1 1 0 0 1-1-1v-2a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v2a1 1 0 0 1-1 1z"}],["path",{d:"M17 21v-2"}],["path",{d:"M19 14V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V10"}],["path",{d:"M21 21v-2"}],["path",{d:"M3 5V3"}],["path",{d:"M4 10a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2a2 2 0 0 1-2 2z"}],["path",{d:"M7 5V3"}]];_t(e,mt({name:"cable"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function _d(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 3v16a2 2 0 0 0 2 2h16"}],["path",{d:"M18 17V9"}],["path",{d:"M13 17V5"}],["path",{d:"M8 17v-3"}]];_t(e,mt({name:"chart-column"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function xd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["line",{x1:"12",x2:"12",y1:"8",y2:"12"}],["line",{x1:"12",x2:"12.01",y1:"16",y2:"16"}]];_t(e,mt({name:"circle-alert"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function kd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M21.801 10A10 10 0 1 1 17 3.335"}],["path",{d:"m9 11 3 3L22 4"}]];_t(e,mt({name:"circle-check-big"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function wd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 6v6l4 2"}]];_t(e,mt({name:"clock"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Sd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m18 16 4-4-4-4"}],["path",{d:"m6 8-4 4 4 4"}],["path",{d:"m14.5 4-5 16"}]];_t(e,mt({name:"code-xml"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Ad(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["ellipse",{cx:"12",cy:"5",rx:"9",ry:"3"}],["path",{d:"M3 5V19A9 3 0 0 0 21 19V5"}],["path",{d:"M3 12A9 3 0 0 0 21 12"}]];_t(e,mt({name:"database"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Ed(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["line",{x1:"12",x2:"12",y1:"2",y2:"22"}],["path",{d:"M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"}]];_t(e,mt({name:"dollar-sign"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function $d(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M15 6a9 9 0 0 0-9 9V3"}],["circle",{cx:"18",cy:"6",r:"3"}],["circle",{cx:"6",cy:"18",r:"3"}]];_t(e,mt({name:"git-branch"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Cd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"10"}],["path",{d:"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"}],["path",{d:"M2 12h20"}]];_t(e,mt({name:"globe"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Md(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5"}],["path",{d:"M3.22 13H9.5l.5-1 2 4.5 2-7 1.5 3.5h5.27"}]];_t(e,mt({name:"heart-pulse"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Nd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M12 2v4"}],["path",{d:"m16.2 7.8 2.9-2.9"}],["path",{d:"M18 12h4"}],["path",{d:"m16.2 16.2 2.9 2.9"}],["path",{d:"M12 18v4"}],["path",{d:"m4.9 19.1 2.9-2.9"}],["path",{d:"M2 12h4"}],["path",{d:"m4.9 4.9 2.9 2.9"}]];_t(e,mt({name:"loader"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Td(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z"}]];_t(e,mt({name:"message-square"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Pd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401"}]];_t(e,mt({name:"moon"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Od(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"}]];_t(e,mt({name:"paperclip"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function To(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"}],["path",{d:"M21 3v5h-5"}],["path",{d:"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"}],["path",{d:"M8 16H3v5"}]];_t(e,mt({name:"refresh-cw"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Id(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"m21 21-4.34-4.34"}],["circle",{cx:"11",cy:"11",r:"8"}]];_t(e,mt({name:"search"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Ld(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"}],["circle",{cx:"12",cy:"12",r:"3"}]];_t(e,mt({name:"settings"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Fd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"}]];_t(e,mt({name:"shield"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function Rd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["circle",{cx:"12",cy:"12",r:"4"}],["path",{d:"M12 2v2"}],["path",{d:"M12 20v2"}],["path",{d:"m4.93 4.93 1.41 1.41"}],["path",{d:"m17.66 17.66 1.41 1.41"}],["path",{d:"M2 12h2"}],["path",{d:"M20 12h2"}],["path",{d:"m6.34 17.66-1.41 1.41"}],["path",{d:"m19.07 4.93-1.41 1.41"}]];_t(e,mt({name:"sun"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}function jd(e,t){Ce(t,!0);/**
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
 */let r=yt(t,["$$slots","$$events","$$legacy"]);const n=[["path",{d:"M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"}]];_t(e,mt({name:"zap"},()=>r,{get iconNode(){return n},children:(s,o)=>{var l=Le(),d=Ae(l);bt(d,()=>t.children??we),f(s,l)},$$slots:{default:!0}})),Me()}var Dd=x('<p class="text-sm text-red-500 dark:text-red-400"> </p>'),Hd=x('<div class="flex min-h-screen items-center justify-center bg-gray-50 px-4 py-8 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><div class="w-full max-w-md rounded-xl border border-gray-200 bg-white p-6 shadow-xl shadow-black/10 dark:border-gray-700 dark:bg-gray-800 dark:shadow-black/30"><div class="flex items-center justify-between gap-3"><h1 class="text-2xl font-semibold tracking-tight"> </h1> <button type="button" class="rounded-lg border border-gray-300 bg-gray-50 px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p> <form class="mt-6 space-y-4"><label class="block text-sm font-medium text-gray-600 dark:text-gray-300" for="token"> </label> <input id="token" type="password" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-gray-900 outline-none ring-sky-500 transition focus:ring-2 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100" autocomplete="off"/> <!> <button type="submit" class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"> </button></form></div></div>');function zd(e,t){Ce(t,!0);let r=L(""),n=L("");function s($){var D;$.preventDefault();const C=a(r).trim();if(!C){b(n,h("login.tokenRequired"),!0);return}Pl(C),b(n,""),(D=t.onLogin)==null||D.call(t,C)}var o=Hd(),l=i(o),d=i(l),c=i(d),u=i(c),y=v(c,2),m=i(y),k=v(d,2),I=i(k),T=v(k,2),F=i(T),M=i(F),O=v(F,2),Z=v(O,2);{var K=$=>{var C=Dd(),D=i(C);N(()=>p(D,a(n))),f($,C)};B(Z,$=>{a(n)&&$(K)})}var E=v(Z,2),g=i(E);N(($,C,D,ue,pe,Ke)=>{p(u,$),pt(y,"aria-label",C),p(m,aa.lang==="zh"?"中文 / EN":"EN / 中文"),p(I,D),p(M,ue),pt(O,"placeholder",pe),p(g,Ke)},[()=>h("login.title"),()=>h("app.language"),()=>h("login.hint"),()=>h("login.accessToken"),()=>h("login.placeholder"),()=>h("login.login")]),J("click",y,function(...$){da==null||da.apply(this,$)}),Er("submit",T,s),Kr(O,()=>a(r),$=>b(r,$)),f(e,o),Me()}fr(["click"]);const Sn="".trim(),rn=Sn.endsWith("/")?Sn.slice(0,-1):Sn;class ws extends Error{constructor(t,r){super(r),this.name="ApiError",this.status=t}}async function Ud(e){return(e.headers.get("content-type")||"").includes("application/json")?e.json().catch(()=>null):e.text().catch(()=>null)}function Bd(e,t){return e&&typeof e=="object"&&typeof e.error=="string"?e.error:`Request failed (${t})`}async function It(e,t={}){const r=Ua(),n={Accept:"application/json",...t.headers};r&&(n.Authorization=`Bearer ${r}`),t.body&&!(t.body instanceof FormData)&&!n["Content-Type"]&&(n["Content-Type"]="application/json");const s=await fetch(`${rn}${e}`,{...t,headers:n}),o=await Ud(s);if(s.status===401)throw $o(),Pr("/",!0),new ws(401,"Unauthorized");if(!s.ok)throw new ws(s.status,Bd(o,s.status));return o}const $t={getStatus:()=>It("/api/status"),getSessions:()=>It("/api/sessions"),getSessionMessages:e=>It(`/api/sessions/${encodeURIComponent(e)}/messages`),sendMessage:(e,t)=>It(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:JSON.stringify({message:t})}),sendMessageWithMedia:(e,t,r=[])=>{if(!Array.isArray(r)||r.length===0)return $t.sendMessage(e,t);const n=new FormData;n.append("message",t);for(const s of r)n.append("files",s);return It(`/api/sessions/${encodeURIComponent(e)}/message`,{method:"POST",body:n})},getSessionMediaUrl:e=>{const t=new URLSearchParams({path:e}),r=Ua();return r&&t.set("token",r),`${rn}/api/sessions/media?${t.toString()}`},getChannelsStatus:()=>It("/api/channels/status"),getConfig:()=>It("/api/config"),saveConfig:e=>It("/api/config",{method:"POST",body:JSON.stringify(e)}),getHooks:()=>It("/api/hooks"),getMcpServers:()=>It("/api/mcp/servers"),getSkills:()=>It("/api/skills"),discoverSkills:(e="github",t="")=>{const r=new URLSearchParams;return e&&r.set("source",e),t&&r.set("query",t),It(`/api/skills/discover?${r.toString()}`)},installSkill:(e,t)=>It("/api/skills/install",{method:"POST",body:JSON.stringify({url:e,name:t})}),uninstallSkill:e=>It(`/api/skills/${encodeURIComponent(e)}`,{method:"DELETE"}),toggleSkill:e=>It(`/api/skills/${encodeURIComponent(e)}/toggle`,{method:"PATCH"}),getPlugins:()=>It("/api/plugins"),reloadPlugin:e=>It(`/api/plugins/${encodeURIComponent(e)}/reload`,{method:"POST"})};function Wd(e){if(!Number.isFinite(e)||e<0)return"0s";const t=Math.floor(e/86400),r=Math.floor(e%86400/3600),n=Math.floor(e%3600/60),s=Math.floor(e%60),o=[];return t>0&&o.push(`${t}d`),(r>0||o.length>0)&&o.push(`${r}h`),(n>0||o.length>0)&&o.push(`${n}m`),o.push(`${s}s`),o.join(" ")}var Vd=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),qd=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Gd=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Kd=x('<div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><p class="text-xs uppercase tracking-wide text-gray-500 dark:text-gray-400"> </p> <p class="mt-2 text-lg font-semibold text-gray-900 dark:text-gray-100"> </p></div>'),Jd=x('<p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),Yd=x('<li class="rounded-full border border-gray-300 bg-gray-50 px-3 py-1 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"> </li>'),Xd=x('<ul class="mt-3 flex flex-wrap gap-2"></ul>'),Qd=x('<div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-5"></div> <div class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><h3 class="text-sm font-semibold uppercase tracking-wide text-gray-600 dark:text-gray-300"> </h3> <!></div>',1),Zd=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function ec(e,t){Ce(t,!0);let r=L(null),n=L(!0),s=L(""),o=L("");function l(g){return typeof g!="string"||g.length===0?h("common.unknown"):g.replaceAll("_"," ").split(" ").map($=>$.charAt(0).toUpperCase()+$.slice(1)).join(" ")}function d(g){const $=`channels.names.${g}`,C=h($);return C===$?l(g):C}const c=ne(()=>{var g,$,C,D,ue;return[{label:h("overview.version"),value:((g=a(r))==null?void 0:g.version)??h("common.na")},{label:h("overview.uptime"),value:typeof(($=a(r))==null?void 0:$.uptime_seconds)=="number"?Wd(a(r).uptime_seconds):h("common.na")},{label:h("overview.model"),value:((C=a(r))==null?void 0:C.model)??h("common.na")},{label:h("overview.memoryBackend"),value:((D=a(r))==null?void 0:D.memory_backend)??h("common.na")},{label:h("overview.gatewayPort"),value:(ue=a(r))!=null&&ue.gateway_port?String(a(r).gateway_port):h("common.na")}]}),u=ne(()=>{var g;return Array.isArray((g=a(r))==null?void 0:g.channels)?a(r).channels:[]});async function y(){try{const g=await $t.getStatus();b(r,g,!0),b(s,""),b(o,new Date().toLocaleTimeString(),!0)}catch(g){b(s,g instanceof Error?g.message:h("overview.loadFailed"),!0)}finally{b(n,!1)}}Bt(()=>{let g=!1;const $=async()=>{g||await y()};$();const C=setInterval($,3e4);return()=>{g=!0,clearInterval(C)}});var m=Zd(),k=i(m),I=i(k),T=i(I),F=v(I,2);{var M=g=>{var $=Vd(),C=i($);N(D=>p(C,D),[()=>h("common.updatedAt",{time:a(o)})]),f(g,$)};B(F,g=>{a(o)&&g(M)})}var O=v(k,2);{var Z=g=>{var $=qd(),C=i($);N(D=>p(C,D),[()=>h("overview.loading")]),f(g,$)},K=g=>{var $=Gd(),C=i($);N(()=>p(C,a(s))),f(g,$)},E=g=>{var $=Qd(),C=Ae($);nt(C,21,()=>a(c),dt,(Q,ve)=>{var se=Kd(),Oe=i(se),z=i(Oe),V=v(Oe,2),me=i(V);N(()=>{p(z,a(ve).label),p(me,a(ve).value)}),f(Q,se)});var D=v(C,2),ue=i(D),pe=i(ue),Ke=v(ue,2);{var ze=Q=>{var ve=Jd(),se=i(ve);N(Oe=>p(se,Oe),[()=>h("overview.noChannelsConfigured")]),f(Q,ve)},W=Q=>{var ve=Xd();nt(ve,21,()=>a(u),dt,(se,Oe)=>{var z=Yd(),V=i(z);N(me=>p(V,me),[()=>d(a(Oe))]),f(se,z)}),f(Q,ve)};B(Ke,Q=>{a(u).length===0?Q(ze):Q(W,-1)})}N(Q=>p(pe,Q),[()=>h("overview.configuredChannels")]),f(g,$)};B(O,g=>{a(n)?g(Z):a(s)?g(K,1):g(E,-1)})}N(g=>p(T,g),[()=>h("overview.title")]),f(e,m),Me()}var tc=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),rc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),ac=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),nc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),sc=x('<tr class="cursor-pointer transition hover:bg-gray-50 dark:hover:bg-gray-700/40"><td class="px-4 py-3 font-mono text-xs"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td><td class="px-4 py-3"> </td></tr>'),oc=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><table class="min-w-full divide-y divide-gray-200 text-sm dark:divide-gray-700"><thead class="bg-gray-50 text-left text-gray-600 dark:bg-gray-900/50 dark:text-gray-300"><tr><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th><th class="px-4 py-3 font-semibold"> </th></tr></thead><tbody class="divide-y divide-gray-200 text-gray-700 dark:divide-gray-700 dark:text-gray-200"></tbody></table></div>'),ic=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function lc(e,t){Ce(t,!0);let r=L(Et([])),n=L(!0),s=L(""),o=L("");function l(g){return typeof g!="string"||g.length===0?h("common.unknown"):g.replaceAll("_"," ").split(" ").map($=>$.charAt(0).toUpperCase()+$.slice(1)).join(" ")}function d(g){const $=`channels.names.${g}`,C=h($);return C===$?l(g):C}async function c(){try{const g=await $t.getSessions();b(r,Array.isArray(g)?g:[],!0),b(s,""),b(o,new Date().toLocaleTimeString(),!0)}catch(g){b(s,g instanceof Error?g.message:h("sessions.loadFailed"),!0)}finally{b(n,!1)}}function u(g){Pr(`/chat/${encodeURIComponent(g)}`)}Bt(()=>{let g=!1;const $=async()=>{g||await c()};$();const C=setInterval($,15e3);return()=>{g=!0,clearInterval(C)}});var y=ic(),m=i(y),k=i(m),I=i(k),T=v(k,2);{var F=g=>{var $=tc(),C=i($);N(D=>p(C,D),[()=>h("common.updatedAt",{time:a(o)})]),f(g,$)};B(T,g=>{a(o)&&g(F)})}var M=v(m,2);{var O=g=>{var $=rc(),C=i($);N(D=>p(C,D),[()=>h("sessions.loading")]),f(g,$)},Z=g=>{var $=ac(),C=i($);N(()=>p(C,a(s))),f(g,$)},K=g=>{var $=nc(),C=i($);N(D=>p(C,D),[()=>h("sessions.none")]),f(g,$)},E=g=>{var $=oc(),C=i($),D=i(C),ue=i(D),pe=i(ue),Ke=i(pe),ze=v(pe),W=i(ze),Q=v(ze),ve=i(Q),se=v(Q),Oe=i(se),z=v(se),V=i(z),me=v(D);nt(me,21,()=>a(r),dt,(Fe,oe)=>{var X=sc(),le=i(X),Re=i(le),Je=v(le),ot=i(Je),tt=v(Je),ct=i(tt),R=v(tt),H=i(R),ye=v(R),it=i(ye);N((rt,st)=>{p(Re,a(oe).session_id),p(ot,a(oe).sender),p(ct,rt),p(H,a(oe).message_count),p(it,st)},[()=>d(a(oe).channel),()=>a(oe).last_message_preview||h("common.empty")]),J("click",X,()=>u(a(oe).session_id)),f(Fe,X)}),N((Fe,oe,X,le,Re)=>{p(Ke,Fe),p(W,oe),p(ve,X),p(Oe,le),p(V,Re)},[()=>h("sessions.sessionId"),()=>h("sessions.sender"),()=>h("sessions.channel"),()=>h("sessions.messages"),()=>h("sessions.lastMessage")]),f(g,$)};B(M,g=>{a(n)?g(O):a(s)?g(Z,1):a(r).length===0?g(K,2):g(E,-1)})}N(g=>p(I,g),[()=>h("sessions.title")]),f(e,y),Me()}fr(["click"]);var dc=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),cc=x('<p class="mb-3 rounded-lg border border-blue-500/40 bg-blue-500/15 px-3 py-2 text-sm text-blue-700 dark:text-blue-200"> </p>'),uc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),fc=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),vc=x('<p class="whitespace-pre-wrap break-words text-sm"> </p>'),gc=x('<img alt="Attachment" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 object-contain dark:border-gray-600/40" loading="lazy"/>'),pc=x('<video controls="" class="mt-2 max-h-80 max-w-full rounded-lg border border-gray-300/40 dark:border-gray-600/40"></video>',2),bc=x("<div></div>"),yc=x('<div class="space-y-3"></div>'),hc=x('<img class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"/>'),mc=x('<video class="h-12 w-12 rounded border border-gray-300 object-cover dark:border-gray-600"></video>',2),_c=x('<div class="flex h-12 w-12 items-center justify-center rounded border border-gray-300 bg-gray-100 text-lg text-gray-700 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200">DOC</div>'),xc=x('<div class="flex items-center gap-2 rounded-md border border-gray-200 bg-white/90 p-2 dark:border-gray-700 dark:bg-gray-800/90"><!> <div class="min-w-0 flex-1"><p class="truncate text-sm text-gray-900 dark:text-gray-100"> </p> <p class="truncate text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded px-2 py-1 text-xs text-gray-600 hover:bg-gray-200 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-white">Remove</button></div>'),kc=x('<div class="mb-3 space-y-2 rounded-lg border border-gray-200 bg-gray-50/70 p-2.5 dark:border-gray-700 dark:bg-gray-900/70"><p class="text-xs text-gray-600 dark:text-gray-300"> </p> <div class="max-h-44 space-y-2 overflow-y-auto pr-1"></div></div>'),wc=x('<section class="flex h-[calc(100vh-10rem)] flex-col gap-4"><div class="flex items-center justify-between"><div class="min-w-0"><h2 class="text-2xl font-semibold"> </h2> <p class="truncate font-mono text-xs text-gray-500 dark:text-gray-400"> </p></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!> <div class="flex min-h-0 flex-1 flex-col rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800" role="region" aria-label="Chat messages"><div><!> <!></div> <form class="border-t border-gray-200 p-3 dark:border-gray-700"><input type="file" class="hidden" multiple="" accept="image/*,video/*,.pdf,.doc,.docx,.txt,.md,.csv,.json,.zip,.tar,.gz,.rar,.ppt,.pptx,.xls,.xlsx"/> <!> <div class="flex items-end gap-2"><textarea rows="2" class="min-h-[2.75rem] flex-1 resize-y rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-900 outline-none focus:border-blue-500 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-100"></textarea> <button type="button" title="Attach files" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-600 hover:border-gray-400 hover:bg-gray-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:border-gray-500 dark:hover:bg-gray-700"><!></button> <button type="submit" class="rounded-lg bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50"> </button></div></form></div></section>');function Sc(e,t){Ce(t,!0);const r=10,n=/\[(IMAGE|VIDEO):([^\]]+)\]|(data:(?:image|video)\/[a-zA-Z0-9.+-]+;base64,[a-zA-Z0-9+/=]+)/gi;let s=la(t,"sessionId",3,""),o=L(Et([])),l=L(""),d=L(!0),c=L(!1),u=L(""),y=L(null),m=L(null),k=L(Et([])),I=L(!1),T=0;function F(){Pr("/sessions")}function M(w){return w==="user"?"ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-blue-600 px-4 py-2 text-white":w==="assistant"?"mr-auto max-w-[85%] rounded-2xl rounded-bl-md bg-gray-200 px-4 py-2 text-gray-900 dark:bg-gray-700 dark:text-gray-100":"mx-auto max-w-[90%] rounded-lg bg-gray-100/60 px-3 py-1.5 text-center text-xs text-gray-500 dark:bg-gray-800/60 dark:text-gray-400"}function O(w){return((w==null?void 0:w.type)||"").startsWith("image/")}function Z(w){return((w==null?void 0:w.type)||"").startsWith("video/")}function K(w){if(!Number.isFinite(w)||w<=0)return"0 B";const q=["B","KB","MB","GB"];let ee=w,de=0;for(;ee>=1024&&de<q.length-1;)ee/=1024,de+=1;return`${ee.toFixed(de===0?0:1)} ${q[de]}`}function E(w){return typeof w=="string"&&w.trim().length>0?w:"unknown"}function g(w){const q=O(w),ee=Z(w);return{id:`${w.name}-${w.lastModified}-${Math.random().toString(36).slice(2)}`,file:w,name:w.name,size:w.size,type:E(w.type),isImage:q,isVideo:ee,previewUrl:q||ee?URL.createObjectURL(w):""}}function $(w){w&&typeof w.previewUrl=="string"&&w.previewUrl.startsWith("blob:")&&URL.revokeObjectURL(w.previewUrl)}function C(){for(const w of a(k))$(w);b(k,[],!0),a(m)&&(a(m).value="")}function D(w){if(!w||w.length===0||a(c))return;const q=Array.from(w),ee=[],de=Math.max(0,r-a(k).length);for(const kt of q.slice(0,de))ee.push(g(kt));b(k,[...a(k),...ee],!0)}function ue(w){const q=a(k).find(ee=>ee.id===w);q&&$(q),b(k,a(k).filter(ee=>ee.id!==w),!0)}function pe(){var w;a(c)||(w=a(m))==null||w.click()}function Ke(w){var q;D((q=w.currentTarget)==null?void 0:q.files),a(m)&&(a(m).value="")}function ze(w){w.preventDefault(),!a(c)&&(T+=1,b(I,!0))}function W(w){w.preventDefault(),!a(c)&&w.dataTransfer&&(w.dataTransfer.dropEffect="copy")}function Q(w){w.preventDefault(),T=Math.max(0,T-1),T===0&&b(I,!1)}function ve(w){var q;w.preventDefault(),T=0,b(I,!1),D((q=w.dataTransfer)==null?void 0:q.files)}function se(w){const q=(w||"").trim();if(!q)return"";const ee=q.toLowerCase();return ee.startsWith("data:image/")||ee.startsWith("data:video/")||ee.startsWith("http://")||ee.startsWith("https://")?q:$t.getSessionMediaUrl(q)}function Oe(w,q){const ee=(q||"").trim().toLowerCase();return w==="VIDEO"||ee.startsWith("data:video/")?"video":ee.startsWith("data:image/")?"image":[".mp4",".webm",".mov",".m4v",".ogg"].some(kt=>ee.endsWith(kt))?"video":"image"}function z(w){if(typeof w!="string"||w.length===0)return[];const q=[];n.lastIndex=0;let ee=0,de;for(;(de=n.exec(w))!==null;){de.index>ee&&q.push({id:`text-${ee}`,kind:"text",value:w.slice(ee,de.index)});const kt=(de[1]||"").toUpperCase(),Ze=(de[2]||de[3]||"").trim();if(Ze){const Be=Oe(kt,Ze);q.push({id:`${Be}-${de.index}`,kind:Be,value:Ze})}ee=n.lastIndex}return ee<w.length&&q.push({id:`text-tail-${ee}`,kind:"text",value:w.slice(ee)}),q}async function V(){await bo(),a(y)&&(a(y).scrollTop=a(y).scrollHeight)}async function me(){try{const w=await $t.getSessionMessages(s());b(o,Array.isArray(w)?w:[],!0),b(u,""),await V()}catch(w){b(u,w instanceof Error?w.message:h("chat.loadFailed"),!0)}finally{b(d,!1)}}async function Fe(){const w=a(l).trim(),q=a(k).map(de=>de.file);if(w.length===0&&q.length===0||a(c))return;b(c,!0),b(l,""),b(u,"");const ee=q.length>0;ee||(b(o,[...a(o),{role:"user",content:w}],!0),await V());try{const de=ee?await $t.sendMessageWithMedia(s(),w,q):await $t.sendMessage(s(),w);ee?await me():de&&typeof de.reply=="string"&&de.reply.length>0&&b(o,[...a(o),{role:"assistant",content:de.reply}],!0),C()}catch(de){b(u,de instanceof Error?de.message:h("chat.sendFailed"),!0),await me()}finally{b(c,!1),await V()}}function oe(w){w.preventDefault(),Fe()}Bt(()=>{let w=!1;return(async()=>{w||(b(d,!0),await me())})(),()=>{w=!0}}),Ml(()=>{for(const w of a(k))$(w)});var X=wc(),le=i(X),Re=i(le),Je=i(Re),ot=i(Je),tt=v(Je,2),ct=i(tt),R=v(Re,2),H=i(R),ye=v(le,2);{var it=w=>{var q=dc(),ee=i(q);N(()=>p(ee,a(u))),f(w,q)};B(ye,w=>{a(u)&&w(it)})}var rt=v(ye,2),st=i(rt),xt=i(st);{var je=w=>{var q=cc(),ee=i(q);N(()=>p(ee,`Drop files to attach (${a(k).length??""}/10 selected)`)),f(w,q)};B(xt,w=>{a(I)&&w(je)})}var Ye=v(xt,2);{var vt=w=>{var q=uc(),ee=i(q);N(de=>p(ee,de),[()=>h("chat.loading")]),f(w,q)},Ue=w=>{var q=fc(),ee=i(q);N(de=>p(ee,de),[()=>h("chat.empty")]),f(w,q)},wt=w=>{var q=yc();nt(q,21,()=>a(o),dt,(ee,de)=>{var kt=bc();nt(kt,21,()=>z(a(de).content),Ze=>Ze.id,(Ze,Be)=>{var _=Le(),S=Ae(_);{var P=ae=>{var ge=Le(),xe=Ae(ge);{var Ee=_e=>{var ie=vc(),Ie=i(ie);N(()=>p(Ie,a(Be).value)),f(_e,ie)},lt=ne(()=>a(Be).value.trim().length>0);B(xe,_e=>{a(lt)&&_e(Ee)})}f(ae,ge)},U=ae=>{var ge=gc();N(xe=>pt(ge,"src",xe),[()=>se(a(Be).value)]),f(ae,ge)},re=ae=>{var ge=pc();N(xe=>pt(ge,"src",xe),[()=>se(a(Be).value)]),f(ae,ge)};B(S,ae=>{a(Be).kind==="text"?ae(P):a(Be).kind==="image"?ae(U,1):a(Be).kind==="video"&&ae(re,2)})}f(Ze,_)}),N(Ze=>et(kt,1,Ze),[()=>wo(M(a(de).role))]),f(ee,kt)}),f(w,q)};B(Ye,w=>{a(d)?w(vt):a(o).length===0?w(Ue,1):w(wt,-1)})}zn(st,w=>b(y,w),()=>a(y));var fe=v(st,2),Ne=i(fe);zn(Ne,w=>b(m,w),()=>a(m));var he=v(Ne,2);{var Te=w=>{var q=kc(),ee=i(q),de=i(ee),kt=v(ee,2);nt(kt,21,()=>a(k),Ze=>Ze.id,(Ze,Be)=>{var _=xc(),S=i(_);{var P=ie=>{var Ie=hc();N(()=>{pt(Ie,"src",a(Be).previewUrl),pt(Ie,"alt",a(Be).name)}),f(ie,Ie)},U=ie=>{var Ie=mc();Ie.muted=!0,N(()=>pt(Ie,"src",a(Be).previewUrl)),f(ie,Ie)},re=ie=>{var Ie=_c();f(ie,Ie)};B(S,ie=>{a(Be).isImage?ie(P):a(Be).isVideo?ie(U,1):ie(re,-1)})}var ae=v(S,2),ge=i(ae),xe=i(ge),Ee=v(ge,2),lt=i(Ee),_e=v(ae,2);N(ie=>{p(xe,a(Be).name),p(lt,`${a(Be).type??""} · ${ie??""}`)},[()=>K(a(Be).size)]),J("click",_e,()=>ue(a(Be).id)),f(Ze,_)}),N(()=>p(de,`Attachments (${a(k).length??""}/10)`)),f(w,q)};B(he,w=>{a(k).length>0&&w(Te)})}var j=v(he,2),G=i(j),qe=v(G,2),at=i(qe);Od(at,{size:16});var ut=v(qe,2),St=i(ut);N((w,q,ee,de,kt,Ze)=>{p(ot,w),p(ct,`${q??""}: ${s()??""}`),p(H,ee),et(st,1,`flex-1 overflow-y-auto p-4 ${a(I)?"bg-blue-500/10 ring-1 ring-inset ring-blue-500/40":""}`),pt(G,"placeholder",de),qe.disabled=a(c)||a(k).length>=r,ut.disabled=kt,p(St,Ze)},[()=>h("chat.title"),()=>h("chat.session"),()=>h("chat.back"),()=>h("chat.inputPlaceholder"),()=>a(c)||!a(l).trim()&&a(k).length===0,()=>a(c)?h("chat.sending"):h("chat.send")]),J("click",R,F),Er("dragenter",rt,ze),Er("dragover",rt,W),Er("dragleave",rt,Q),Er("drop",rt,ve),Er("submit",fe,oe),J("change",Ne,Ke),Kr(G,()=>a(l),w=>b(l,w)),J("click",qe,pe),f(e,X),Me()}fr(["click","change"]);var Ac=x('<span class="text-xs text-gray-500 dark:text-gray-400"> </span>'),Ec=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),$c=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Cc=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Mc=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-3 text-sm text-gray-500 dark:text-gray-400"> </p></article>'),Nc=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),Tc=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <!></div> <!></section>');function Pc(e,t){Ce(t,!0);let r=L(Et([])),n=L(!0),s=L(""),o=L("");function l(E){return typeof E!="string"||E.length===0?h("common.unknown"):E.replaceAll("_"," ").split(" ").map(g=>g.charAt(0).toUpperCase()+g.slice(1)).join(" ")}function d(E){const g=`channels.names.${E}`,$=h(g);return $===g?l(E):$}async function c(){try{const E=await $t.getChannelsStatus();b(r,Array.isArray(E==null?void 0:E.channels)?E.channels:[],!0),b(s,""),b(o,new Date().toLocaleTimeString(),!0)}catch(E){b(s,E instanceof Error?E.message:h("channels.loadFailed"),!0)}finally{b(n,!1)}}Bt(()=>{let E=!1;const g=async()=>{E||await c()};g();const $=setInterval(g,3e4);return()=>{E=!0,clearInterval($)}});var u=Tc(),y=i(u),m=i(y),k=i(m),I=v(m,2);{var T=E=>{var g=Ac(),$=i(g);N(C=>p($,C),[()=>h("common.updatedAt",{time:a(o)})]),f(E,g)};B(I,E=>{a(o)&&E(T)})}var F=v(y,2);{var M=E=>{var g=Ec(),$=i(g);N(C=>p($,C),[()=>h("channels.loading")]),f(E,g)},O=E=>{var g=$c(),$=i(g);N(()=>p($,a(s))),f(E,g)},Z=E=>{var g=Cc(),$=i(g);N(C=>p($,C),[()=>h("channels.noChannels")]),f(E,g)},K=E=>{var g=Nc();nt(g,21,()=>a(r),dt,($,C)=>{var D=Mc(),ue=i(D),pe=i(ue),Ke=i(pe),ze=v(pe,2),W=i(ze),Q=v(ue,2),ve=i(Q);N((se,Oe,z,V)=>{p(Ke,se),et(ze,1,`rounded-full px-2 py-1 text-xs font-medium ${a(C).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(W,Oe),p(ve,`${z??""}: ${V??""}`)},[()=>d(a(C).name),()=>a(C).enabled?h("common.enabled"):h("common.disabled"),()=>h("channels.type"),()=>d(a(C).type)]),f($,D)}),f(E,g)};B(F,E=>{a(n)?E(M):a(s)?E(O,1):a(r).length===0?E(Z,2):E(K,-1)})}N(E=>p(k,E),[()=>h("channels.title")]),f(e,u),Me()}function An(e){return e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;")}const Ss=/(\"(\\u[0-9a-fA-F]{4}|\\[^u]|[^\\\"])*\"(?:\s*:)?|\btrue\b|\bfalse\b|\bnull\b|-?\d+(?:\.\d+)?(?:[eE][+\-]?\d+)?)/g;function Oc(e){return e.startsWith('"')?e.endsWith(":")?"text-sky-300":"text-emerald-300":e==="true"||e==="false"?"text-amber-300":e==="null"?"text-fuchsia-300":"text-violet-300"}function Ic(e){if(!e)return"";let t="",r=0;Ss.lastIndex=0;for(const n of e.matchAll(Ss)){const s=n.index??0,o=n[0];t+=An(e.slice(r,s)),t+=`<span class="${Oc(o)}">${An(o)}</span>`,r=s+o.length}return t+=An(e.slice(r)),t}var Lc=x('<span class="ml-1.5 text-xs text-sky-500 dark:text-sky-400">已修改</span>'),Fc=x('<button type="button"><span></span></button>'),Rc=x("<option> </option>"),jc=x('<select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select>'),Dc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Hc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 bg-white px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800 dark:text-red-400">×</button></div>'),zc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600 dark:text-gray-400 dark:hover:border-sky-500 dark:hover:text-sky-400">+ 添加</button></div>'),Uc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 hover:text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400 dark:hover:text-gray-200"> </button></div>'),Bc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-700 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Wc=x('<div><div class="flex items-start justify-between gap-3"><div class="flex-1 min-w-0"><label class="block text-sm font-medium text-gray-700 dark:text-gray-200"> <!></label> <p class="mt-0.5 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex-shrink-0 w-64"><!></div></div></div>'),Vc=x('<button type="button"><span></span></button>'),qc=x('<input type="number" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/>'),Gc=x('<div class="flex gap-1"><input type="text" class="flex-1 rounded border border-gray-300 bg-white px-2 py-1 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/> <button type="button" class="rounded border border-gray-300 px-2 py-1 text-xs text-red-500 hover:bg-red-500/10 dark:border-gray-600 dark:bg-gray-800">×</button></div>'),Kc=x('<div class="space-y-1.5"><!> <button type="button" class="rounded border border-dashed border-gray-300 px-2 py-1 text-xs text-gray-500 hover:border-sky-500 hover:text-sky-500 dark:border-gray-600">+ 添加</button></div>'),Jc=x('<div class="flex gap-1"><input class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 text-xs text-gray-500 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-400"> </button></div>'),Yc=x('<input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200" placeholder="未设置"/>'),Xc=x('<textarea class="w-full rounded-lg border border-gray-300 bg-white font-mono text-xs leading-relaxed p-2 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200 resize-y"></textarea>'),Qc=x('<span class="text-xs text-sky-500">已修改</span>'),Zc=x('<div class="mb-2 flex items-center gap-2"><!> <span class="font-mono text-xs font-medium text-gray-600 dark:text-gray-300"> </span> <!></div> <!>',1),eu=x('<span class="ml-1.5 text-xs text-sky-500">已修改</span>'),tu=x('<div class="flex items-center justify-between gap-3"><span class="min-w-0 flex-1 font-mono text-sm text-gray-700 dark:text-gray-200"> <!></span> <div class="w-56 flex-shrink-0"><!></div></div>'),ru=x("<div><!></div>"),au=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),nu=x('<span class="ml-auto text-xs text-gray-400"> </span>'),su=x('<details class="rounded-lg border border-gray-200 dark:border-gray-700"><summary class="cursor-pointer select-none flex items-center gap-2 px-3 py-2 text-sm font-medium text-gray-700 dark:text-gray-200 hover:bg-gray-50 dark:hover:bg-gray-700/50 rounded-lg"><span class="font-mono"> </span> <!> <!></summary> <div class="border-t border-gray-200 px-3 py-2 space-y-2 dark:border-gray-700"><!></div></details>'),ou=x('<div class="space-y-2"></div>'),iu=x('<p class="text-sm text-gray-500 dark:text-gray-400">加载配置中...</p>'),lu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),du=x('<div class="overflow-x-auto rounded-xl border border-gray-200 bg-gray-50 p-4 dark:border-gray-700 dark:bg-gray-950"><pre class="text-sm leading-6 text-gray-700 dark:text-gray-200"><code><!></code></pre></div>'),cu=x('<span class="rounded-full bg-gray-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-gray-500 dark:bg-gray-700 dark:text-gray-300">Auto</span>'),uu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),fu=x('<button type="button"><span> </span> <!> <!></button>'),vu=x('<span class="ml-2 inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),gu=x('<div class="mt-2 border-t border-gray-100 pt-3 dark:border-gray-700/60"><p class="mb-2 text-xs font-medium uppercase tracking-wider text-gray-400 dark:text-gray-500">其他子配置</p> <div class="space-y-2"></div></div>'),pu=x('<details class="group scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 text-base font-semibold text-gray-900 flex items-center gap-2 dark:text-gray-100"><!> <span> </span> <!></summary> <div class="border-t border-gray-200 px-4 py-3 space-y-3 dark:border-gray-700"><!> <!></div></details>'),bu=x('<span class="inline-flex h-2 w-2 rounded-full bg-sky-500"></span>'),yu=x('<details class="scroll-mt-24 rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><summary class="cursor-pointer select-none px-4 py-3 flex items-center gap-2 dark:text-gray-100"><!> <span class="font-mono text-sm font-semibold text-gray-800 dark:text-gray-100"> </span> <!> <span class="ml-auto text-xs text-gray-400 dark:text-gray-500"> </span></summary> <div class="border-t border-gray-200 px-4 py-3 dark:border-gray-700"><!></div></details>'),hu=x('<div class="pt-1"><p class="mb-2 px-1 text-xs font-semibold uppercase tracking-wider text-gray-400 dark:text-gray-500">自动发现的配置项</p> <div class="space-y-3"></div></div>'),mu=x('<div class="space-y-3"><div class="sticky top-0 z-20 -mx-1 overflow-x-auto rounded-xl border border-gray-200 bg-white/95 px-3 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex min-w-max items-center gap-2"></div></div> <!> <!></div>'),_u=x('<div class="flex items-start gap-2 text-xs flex-wrap"><span class="flex-shrink-0 text-gray-400 dark:text-gray-500"> </span> <span class="font-medium text-gray-600 dark:text-gray-300"> </span> <span class="text-red-500 line-through dark:text-red-400 break-all"> </span> <span class="text-gray-400 dark:text-gray-600">→</span> <span class="text-green-600 dark:text-green-400 break-all"> </span></div>'),xu=x('<div class="mx-auto mt-3 max-w-5xl rounded-lg border border-gray-200 bg-gray-50 p-3 dark:border-gray-700 dark:bg-gray-950"><p class="mb-2 text-xs font-medium text-gray-500 dark:text-gray-400">变更详情</p> <div class="space-y-1.5 max-h-48 overflow-y-auto"></div></div>'),ku=x('<div class="fixed bottom-0 left-0 right-0 z-50 border-t border-gray-200 bg-white/95 px-6 py-3 backdrop-blur-sm dark:border-gray-700 dark:bg-gray-900/95"><div class="mx-auto flex max-w-5xl items-center justify-between gap-4"><div class="flex items-center gap-3"><span class="text-sm text-sky-600 dark:text-sky-400"> </span> <button type="button" class="text-sm text-gray-500 underline hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"> </button></div> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">放弃修改</button> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"> </button></div></div> <!></div>'),wu=x("<div> </div>"),Su=x('<section class="space-y-4 pb-24"><div class="flex items-center justify-between gap-4"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-sm text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700">复制 JSON</button></div></div> <!> <!> <!></section>');function Au(e,t){Ce(t,!0);const r=(_,S=we,P=we)=>{const U=ne(()=>Oe(S())),re=ne(()=>a(le).has(S())),ae=ne(()=>a(O).has(S()));var ge=Wc(),xe=i(ge),Ee=i(xe),lt=i(Ee),_e=i(lt),ie=v(_e);{var Ie=We=>{var ce=Lc();f(We,ce)};B(ie,We=>{a(re)&&We(Ie)})}var $e=v(lt,2),Y=i($e),te=v(Ee,2),be=i(te);{var De=We=>{var ce=Fc(),Xe=i(ce);N(()=>{et(ce,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${a(U)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(Xe,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${a(U)?"translate-x-6":"translate-x-1"}`)}),J("click",ce,()=>H(S(),!a(U))),f(We,ce)},ke=We=>{var ce=jc();nt(ce,21,()=>P().options,dt,(gt,Ht)=>{var Ge=Rc(),Nt=i(Ge),Wt={};N(()=>{p(Nt,a(Ht)||"(默认)"),Wt!==(Wt=a(Ht))&&(Ge.value=(Ge.__value=a(Ht))??"")}),f(gt,Ge)});var Xe;ts(ce),N(()=>{Xe!==(Xe=a(U)??P().default)&&(ce.value=(ce.__value=a(U)??P().default)??"",za(ce,a(U)??P().default))}),J("change",ce,gt=>H(S(),gt.target.value)),f(We,ce)},ft=We=>{var ce=Dc();N(Xe=>{wr(ce,a(U)??P().default),pt(ce,"min",P().min),pt(ce,"max",P().max),pt(ce,"step",P().step??1),pt(ce,"placeholder",Xe)},[()=>String(P().default)]),J("input",ce,Xe=>{const gt=P().step&&P().step<1?parseFloat(Xe.target.value):parseInt(Xe.target.value,10);isNaN(gt)||H(S(),gt)}),f(We,ce)},ht=We=>{var ce=zc(),Xe=i(ce);{var gt=Nt=>{var Wt=Le(),Nr=Ae(Wt);nt(Nr,17,()=>a(U),dt,(zt,Ka,Na)=>{var sa=Hc(),oa=i(sa),pn=v(oa,2);N(()=>wr(oa,a(Ka))),J("input",oa,bn=>rt(S(),Na,bn.target.value)),J("click",pn,()=>it(S(),Na)),f(zt,sa)}),f(Nt,Wt)},Ht=ne(()=>Array.isArray(a(U)));B(Xe,Nt=>{a(Ht)&&Nt(gt)})}var Ge=v(Xe,2);J("click",Ge,()=>ye(S())),f(We,ce)},Ct=We=>{var ce=Uc(),Xe=i(ce),gt=v(Xe,2),Ht=i(gt);N(()=>{pt(Xe,"type",a(ae)?"text":"password"),wr(Xe,a(U)??""),pt(Xe,"placeholder",P().default||"未设置"),p(Ht,a(ae)?"隐藏":"显示")}),J("input",Xe,Ge=>H(S(),Ge.target.value)),J("click",gt,()=>st(S())),f(We,ce)},Mt=We=>{var ce=Bc();N(()=>{wr(ce,a(U)??""),pt(ce,"placeholder",P().default||"未设置")}),J("input",ce,Xe=>H(S(),Xe.target.value)),f(We,ce)};B(be,We=>{P().type==="bool"?We(De):P().type==="enum"?We(ke,1):P().type==="number"?We(ft,2):P().type==="array"?We(ht,3):P().sensitive?We(Ct,4):We(Mt,-1)})}N(()=>{et(ge,1,`rounded-lg border p-3 transition-colors ${a(re)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`),p(_e,`${P().label??""} `),p(Y,P().desc)}),f(_,ge)},n=(_,S=we,P=we)=>{const U=ne(()=>C(S().split(".").pop()??"")),re=ne(()=>a(O).has(S()));var ae=Le(),ge=Ae(ae);{var xe=$e=>{var Y=Vc(),te=i(Y);N(()=>{et(Y,1,`relative inline-flex h-6 w-11 items-center rounded-full transition-colors ${P()?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(te,1,`inline-block h-4 w-4 transform rounded-full bg-white transition-transform ${P()?"translate-x-6":"translate-x-1"}`)}),J("click",Y,()=>H(S(),!P())),f($e,Y)},Ee=$e=>{var Y=qc();N(()=>wr(Y,P())),J("input",Y,te=>{const be=parseFloat(te.target.value);isNaN(be)||H(S(),be)}),f($e,Y)},lt=$e=>{var Y=Kc(),te=i(Y);nt(te,17,P,dt,(De,ke,ft)=>{var ht=Gc(),Ct=i(ht),Mt=v(Ct,2);N(()=>wr(Ct,a(ke))),J("input",Ct,We=>{const ce=[...ve(a(c),S())||[]];ce[ft]=We.target.value,H(S(),ce)}),J("click",Mt,()=>{const We=(ve(a(c),S())||[]).filter((ce,Xe)=>Xe!==ft);H(S(),We)}),f(De,ht)});var be=v(te,2);J("click",be,()=>{const De=[...ve(a(c),S())||[],""];H(S(),De)}),f($e,Y)},_e=ne(()=>Array.isArray(P())),ie=$e=>{var Y=Jc(),te=i(Y),be=v(te,2),De=i(be);N(()=>{pt(te,"type",a(re)?"text":"password"),wr(te,P()??""),p(De,a(re)?"隐藏":"显示")}),J("input",te,ke=>H(S(),ke.target.value)),J("click",be,()=>st(S())),f($e,Y)},Ie=$e=>{var Y=Yc();N(()=>wr(Y,P()??"")),J("input",Y,te=>H(S(),te.target.value)),f($e,Y)};B(ge,$e=>{typeof P()=="boolean"?$e(xe):typeof P()=="number"?$e(Ee,1):a(_e)?$e(lt,2):a(U)?$e(ie,3):$e(Ie,-1)})}f(_,ae)},s=(_,S=we,P=we)=>{const U=ne(()=>JSON.stringify(P(),null,2)),re=ne(()=>Math.min(15,(a(U).match(/\n/g)||[]).length+2));var ae=Xc();N(()=>{wr(ae,a(U)),pt(ae,"rows",a(re))}),Er("blur",ae,ge=>{try{const xe=JSON.parse(ge.target.value);H(S(),xe)}catch{ge.target.value=JSON.stringify(ve(a(c),S())??P(),null,2)}}),f(_,ae)},o=(_,S=we,P=we,U=we)=>{const re=ne(()=>ve(a(c),S())??U()),ae=ne(()=>a(le).has(S()));var ge=ru(),xe=i(ge);{var Ee=ie=>{var Ie=Zc(),$e=Ae(Ie),Y=i($e);Sd(Y,{size:13,class:"flex-shrink-0 text-gray-400"});var te=v(Y,2),be=i(te),De=v(te,2);{var ke=ht=>{var Ct=Qc();f(ht,Ct)};B(De,ht=>{a(ae)&&ht(ke)})}var ft=v($e,2);s(ft,S,()=>a(re)),N(()=>p(be,P())),f(ie,Ie)},lt=ne(()=>je(a(re))),_e=ie=>{var Ie=tu(),$e=i(Ie),Y=i($e),te=v(Y);{var be=ft=>{var ht=eu();f(ft,ht)};B(te,ft=>{a(ae)&&ft(be)})}var De=v($e,2),ke=i(De);n(ke,S,()=>a(re)),N(()=>p(Y,`${P()??""} `)),f(ie,Ie)};B(xe,ie=>{a(lt)?ie(Ee):ie(_e,-1)})}N(()=>et(ge,1,`rounded-lg border p-3 transition-colors ${a(ae)?"border-sky-500/50 bg-sky-500/5":"border-gray-200 bg-gray-50/40 dark:border-gray-700 dark:bg-gray-900/40"}`)),f(_,ge)},l=(_,S=we,P=we,U=we)=>{const re=ne(()=>g(U())),ae=ne(()=>Re(S()));var ge=su(),xe=i(ge),Ee=i(xe),lt=i(Ee),_e=v(Ee,2);{var ie=ke=>{var ft=au();f(ke,ft)};B(_e,ke=>{a(ae)&&ke(ie)})}var Ie=v(_e,2);{var $e=ke=>{var ft=nu(),ht=i(ft);N(Ct=>p(ht,Ct),[()=>$(U())]),f(ke,ft)};B(Ie,ke=>{a(re)||ke($e)})}var Y=v(xe,2),te=i(Y);{var be=ke=>{var ft=Le(),ht=Ae(ft);nt(ht,17,()=>Object.entries(U()),dt,(Ct,Mt)=>{var We=ne(()=>La(a(Mt),2));let ce=()=>a(We)[0],Xe=()=>a(We)[1];const gt=ne(()=>`${S()}.${ce()}`);var Ht=Le(),Ge=Ae(Ht);{var Nt=zt=>{o(zt,()=>a(gt),ce,Xe)},Wt=ne(()=>g(Xe())),Nr=zt=>{o(zt,()=>a(gt),ce,Xe)};B(Ge,zt=>{a(Wt)?zt(Nt):zt(Nr,-1)})}f(Ct,Ht)}),f(ke,ft)},De=ke=>{o(ke,S,P,U)};B(te,ke=>{a(re)?ke(be):ke(De,-1)})}N(()=>p(lt,P())),f(_,ge)},d=(_,S=we,P=we)=>{var U=Le(),re=Ae(U);{var ae=_e=>{var ie=ou();nt(ie,21,()=>Object.entries(P()),dt,(Ie,$e)=>{var Y=ne(()=>La(a($e),2));let te=()=>a(Y)[0],be=()=>a(Y)[1];var De=Le(),ke=Ae(De);{var ft=Mt=>{l(Mt,()=>`${S()}.${te()}`,te,be)},ht=ne(()=>g(be())),Ct=Mt=>{o(Mt,()=>`${S()}.${te()}`,te,be)};B(ke,Mt=>{a(ht)?Mt(ft):Mt(Ct,-1)})}f(Ie,De)}),f(_e,ie)},ge=ne(()=>g(P())),xe=_e=>{o(_e,S,S,P)},Ee=ne(()=>Array.isArray(P())),lt=_e=>{o(_e,S,S,P)};B(re,_e=>{a(ge)?_e(ae):a(Ee)?_e(xe,1):_e(lt,-1)})}f(_,U)};let c=L(null),u=L(null),y=L(null),m=L(!0),k=L(!1),I=L(""),T=L(""),F=L(!1),M=L(!1),O=L(Et(new Set)),Z=L("provider");const K={provider:jd,gateway:Cd,channels:Td,agent:yd,memory:hd,security:Fd,heartbeat:Md,reliability:To,scheduler:wd,sessions_spawn:$d,observability:_d,web_search:Id,cost:Ed,runtime:Ld,tunnel:md,identity:bd},E={provider:{label:"Provider 设置",defaultOpen:!0,fields:{api_key:{type:"string",sensitive:!0,label:"API Key",desc:"当前 Provider 的 API 密钥。修改后需要重启生效",default:""},api_url:{type:"string",label:"API URL",desc:"自定义 API 端点地址。留空使用 Provider 默认值（如 Ollama 填 http://localhost:11434）",default:""},default_provider:{type:"enum",label:"默认 Provider",desc:"选择 AI 模型提供商。决定使用哪个 API 来处理请求",default:"openrouter",options:["openrouter","anthropic","openai","ollama","gemini","groq","glm","xai","compatible","copilot","claude-cli","dashscope","dashscope-coding-intl","deepseek","fireworks","mistral","together"]},default_model:{type:"string",label:"默认模型",desc:"默认使用的模型名称（如 anthropic/claude-sonnet-4-6）",default:"anthropic/claude-sonnet-4.6"},default_temperature:{type:"number",label:"温度",desc:"模型输出的随机性（0=确定性，2=最随机）。推荐日常对话 0.7，代码任务 0.3",default:.7,min:0,max:2,step:.1}}},gateway:{label:"Gateway 网关",defaultOpen:!0,fields:{"gateway.port":{type:"number",label:"端口",desc:"Gateway HTTP 服务端口号",default:3e3,min:1,max:65535},"gateway.host":{type:"string",label:"监听地址",desc:"绑定的 IP 地址。127.0.0.1 仅本机访问，0.0.0.0 允许外部访问",default:"127.0.0.1"},"gateway.require_pairing":{type:"bool",label:"需要配对",desc:"开启后必须先配对才能访问 API。关闭则任何人可直接访问（不安全）",default:!0},"gateway.allow_public_bind":{type:"bool",label:"允许公网绑定",desc:"允许绑定到非 localhost 地址而不需要隧道。通常不建议开启",default:!1},"gateway.trust_forwarded_headers":{type:"bool",label:"信任代理头",desc:"信任 X-Forwarded-For / X-Real-IP 头。仅在反向代理后方启用",default:!1},"gateway.request_timeout_secs":{type:"number",label:"请求超时(秒)",desc:"HTTP 请求处理超时时间",default:60,min:5,max:600},"gateway.pair_rate_limit_per_minute":{type:"number",label:"配对速率限制(/分)",desc:"每客户端每分钟最大配对请求数",default:10,min:1,max:100},"gateway.webhook_rate_limit_per_minute":{type:"number",label:"Webhook 速率限制(/分)",desc:"每客户端每分钟最大 Webhook 请求数",default:60,min:1,max:1e3}}},channels:{label:"消息通道",defaultOpen:!0,fields:{"channels_config.message_timeout_secs":{type:"number",label:"消息处理超时(秒)",desc:"单条消息处理的最大超时时间（LLM + 工具调用）",default:300,min:30,max:3600},"channels_config.cli":{type:"bool",label:"CLI 交互模式",desc:"启用命令行交互通道",default:!0}}},agent:{label:"Agent 编排",defaultOpen:!1,fields:{"agent.max_tool_iterations":{type:"number",label:"最大工具循环次数",desc:"每条用户消息最多执行多少轮工具调用。设 0 回退到默认 10",default:10,min:0,max:100},"agent.max_history_messages":{type:"number",label:"最大历史消息数",desc:"每个会话保留的历史消息条数",default:50,min:5,max:500},"agent.parallel_tools":{type:"bool",label:"并行工具执行",desc:"允许在单次迭代中并行调用多个工具",default:!1},"agent.compact_context":{type:"bool",label:"紧凑上下文",desc:"为小模型（13B 以下）减少上下文大小",default:!1},"agent.compaction.mode":{type:"enum",label:"上下文压缩模式",desc:"off=不压缩，safeguard=保守压缩（默认），aggressive=激进截断",default:"safeguard",options:["off","safeguard","aggressive"]},"agent.compaction.max_context_tokens":{type:"number",label:"最大上下文 Token",desc:"触发压缩的 Token 阈值",default:128e3,min:1e3,max:1e6},"agent.compaction.keep_recent_messages":{type:"number",label:"压缩后保留消息数",desc:"压缩后保留最近的非系统消息数量",default:12,min:1,max:100},"agent.compaction.memory_flush":{type:"bool",label:"压缩前刷新记忆",desc:"在压缩之前提取并保存记忆",default:!0}}},memory:{label:"记忆存储",defaultOpen:!1,fields:{"memory.backend":{type:"enum",label:"存储后端",desc:"记忆存储引擎类型",default:"sqlite",options:["sqlite","postgres","markdown","lucid","none"]},"memory.auto_save":{type:"bool",label:"自动保存",desc:"自动保存用户输入到记忆",default:!0},"memory.hygiene_enabled":{type:"bool",label:"记忆清理",desc:"定期运行记忆归档和保留清理",default:!0},"memory.archive_after_days":{type:"number",label:"归档天数",desc:"超过此天数的日志/会话文件将被归档",default:7,min:1,max:365},"memory.purge_after_days":{type:"number",label:"清除天数",desc:"归档文件超过此天数后被清除",default:30,min:1,max:3650},"memory.conversation_retention_days":{type:"number",label:"对话保留天数",desc:"SQLite 后端：超过此天数的对话记录被清理",default:3,min:1,max:365},"memory.embedding_provider":{type:"enum",label:"嵌入提供商",desc:"记忆向量化的嵌入模型提供商",default:"none",options:["none","openai","custom"]},"memory.embedding_model":{type:"string",label:"嵌入模型",desc:"嵌入模型名称（如 text-embedding-3-small）",default:"text-embedding-3-small"},"memory.embedding_dimensions":{type:"number",label:"嵌入维度",desc:"嵌入向量的维度数",default:1536,min:64,max:4096},"memory.vector_weight":{type:"number",label:"向量权重",desc:"混合搜索中向量相似度的权重（0-1）",default:.7,min:0,max:1,step:.1},"memory.keyword_weight":{type:"number",label:"关键词权重",desc:"混合搜索中 BM25 关键词匹配的权重（0-1）",default:.3,min:0,max:1,step:.1},"memory.min_relevance_score":{type:"number",label:"最低相关性分数",desc:"低于此分数的记忆不会注入上下文",default:.4,min:0,max:1,step:.05},"memory.snapshot_enabled":{type:"bool",label:"记忆快照",desc:"定期将核心记忆导出为 MEMORY_SNAPSHOT.md",default:!1},"memory.auto_hydrate":{type:"bool",label:"自动恢复",desc:"当 brain.db 不存在时自动从快照恢复",default:!0}}},security:{label:"安全策略",defaultOpen:!1,fields:{"autonomy.level":{type:"enum",label:"自主级别",desc:"read_only=只读，supervised=需审批（默认），full=完全自主",default:"supervised",options:["read_only","supervised","full"]},"autonomy.workspace_only":{type:"bool",label:"仅工作区",desc:"限制文件写入和命令执行在工作区目录内",default:!0},"autonomy.max_actions_per_hour":{type:"number",label:"每小时最大操作数",desc:"每小时允许的最大操作次数",default:20,min:1,max:1e4},"autonomy.require_approval_for_medium_risk":{type:"bool",label:"中风险需审批",desc:"中等风险的 Shell 命令需要明确批准",default:!0},"autonomy.block_high_risk_commands":{type:"bool",label:"阻止高风险命令",desc:"即使在白名单中也阻止高风险命令",default:!0},"autonomy.allowed_commands":{type:"array",label:"允许的命令",desc:"允许执行的命令白名单",default:["git","npm","cargo","ls","cat","grep","find","echo"]},"secrets.encrypt":{type:"bool",label:"加密密钥",desc:"对 config.toml 中的 API Key 和 Token 进行加密存储",default:!0}}},heartbeat:{label:"心跳检测",defaultOpen:!1,fields:{"heartbeat.enabled":{type:"bool",label:"启用心跳",desc:"启用定期心跳检查",default:!1},"heartbeat.interval_minutes":{type:"number",label:"间隔(分钟)",desc:"心跳检查的时间间隔",default:30,min:1,max:1440},"heartbeat.active_hours":{type:"array",label:"活跃时段",desc:"心跳检查的有效小时范围（如 [8, 23]）",default:[8,23]},"heartbeat.prompt":{type:"string",label:"心跳提示词",desc:"心跳触发时使用的提示词",default:"Check HEARTBEAT.md and follow instructions."}}},reliability:{label:"可靠性",defaultOpen:!1,fields:{"reliability.provider_retries":{type:"number",label:"Provider 重试次数",desc:"调用 Provider 失败后的重试次数",default:2,min:0,max:10},"reliability.provider_backoff_ms":{type:"number",label:"重试退避(ms)",desc:"Provider 重试的基础退避时间",default:500,min:100,max:3e4},"reliability.fallback_providers":{type:"array",label:"备用 Provider",desc:"主 Provider 不可用时按顺序尝试的备用列表",default:[]},"reliability.api_keys":{type:"array",label:"轮换 API Key",desc:"遇到速率限制时轮换使用的额外 API Key",default:[]},"reliability.channel_initial_backoff_secs":{type:"number",label:"通道初始退避(秒)",desc:"通道/守护进程重启的初始退避时间",default:2,min:1,max:60},"reliability.channel_max_backoff_secs":{type:"number",label:"通道最大退避(秒)",desc:"通道/守护进程重启的最大退避时间",default:60,min:5,max:3600}}},scheduler:{label:"调度器",defaultOpen:!1,fields:{"scheduler.enabled":{type:"bool",label:"启用调度器",desc:"启用内置定时任务调度循环",default:!0},"scheduler.max_tasks":{type:"number",label:"最大任务数",desc:"最多持久化保存的计划任务数量",default:64,min:1,max:1e3},"scheduler.max_concurrent":{type:"number",label:"最大并发数",desc:"每次调度周期内最多执行的任务数",default:4,min:1,max:32},"cron.enabled":{type:"bool",label:"启用 Cron",desc:"启用 Cron 子系统",default:!0},"cron.max_run_history":{type:"number",label:"Cron 历史记录数",desc:"保留的 Cron 运行历史记录条数",default:50,min:10,max:1e3}}},sessions_spawn:{label:"子进程管理",defaultOpen:!1,fields:{"sessions_spawn.default_mode":{type:"enum",label:"默认模式",desc:"子进程默认执行模式",default:"task",options:["task","process"]},"sessions_spawn.max_concurrent":{type:"number",label:"最大并发数",desc:"全局最大并发子进程/任务数",default:4,min:1,max:32},"sessions_spawn.max_spawn_depth":{type:"number",label:"最大嵌套深度",desc:"子进程可以再次 spawn 的最大深度",default:2,min:1,max:10},"sessions_spawn.max_children_per_agent":{type:"number",label:"每父进程最大子数",desc:"每个父会话允许的最大并发子运行数",default:5,min:1,max:20},"sessions_spawn.cleanup_on_complete":{type:"bool",label:"完成后清理",desc:"进程模式完成后删除工作区目录",default:!0}}},observability:{label:"可观测性",defaultOpen:!1,fields:{"observability.backend":{type:"enum",label:"后端",desc:"可观测性后端类型",default:"none",options:["none","log","prometheus","otel"]},"observability.otel_endpoint":{type:"string",label:"OTLP 端点",desc:"OpenTelemetry Collector 端点 URL（仅 otel 后端）",default:""},"observability.otel_service_name":{type:"string",label:"服务名称",desc:"上报给 OTel 的服务名称",default:"openprx"}}},web_search:{label:"网络搜索",defaultOpen:!1,fields:{"web_search.enabled":{type:"bool",label:"启用搜索",desc:"启用网络搜索工具",default:!1},"web_search.provider":{type:"enum",label:"搜索引擎",desc:"搜索提供商。DuckDuckGo 免费无 Key，Brave 需要 API Key",default:"duckduckgo",options:["duckduckgo","brave"]},"web_search.brave_api_key":{type:"string",sensitive:!0,label:"Brave API Key",desc:"Brave Search API 密钥（选 Brave 时必填）",default:""},"web_search.max_results":{type:"number",label:"最大结果数",desc:"每次搜索返回的最大结果数（1-10）",default:5,min:1,max:10},"web_search.fetch_enabled":{type:"bool",label:"启用页面抓取",desc:"允许抓取和提取网页可读内容",default:!0},"web_search.fetch_max_chars":{type:"number",label:"抓取最大字符",desc:"网页抓取返回的最大字符数",default:1e4,min:100,max:1e5}}},cost:{label:"成本控制",defaultOpen:!1,fields:{"cost.enabled":{type:"bool",label:"启用成本追踪",desc:"启用 API 调用成本追踪和预算控制",default:!1},"cost.daily_limit_usd":{type:"number",label:"日限额(USD)",desc:"每日消费上限（美元）",default:10,min:.1,max:1e4,step:.1},"cost.monthly_limit_usd":{type:"number",label:"月限额(USD)",desc:"每月消费上限（美元）",default:100,min:1,max:1e5,step:1},"cost.warn_at_percent":{type:"number",label:"预警百分比",desc:"消费达到限额的多少百分比时发出警告",default:80,min:10,max:100}}},runtime:{label:"运行时",defaultOpen:!1,fields:{"runtime.kind":{type:"enum",label:"运行时类型",desc:"命令执行环境：native=本机，docker=容器隔离",default:"native",options:["native","docker"]},"runtime.reasoning_enabled":{type:"enum",label:"推理模式",desc:"全局推理/思考模式：null=Provider 默认，true=启用，false=禁用",default:"",options:["","true","false"]}}},tunnel:{label:"隧道",defaultOpen:!1,fields:{"tunnel.provider":{type:"enum",label:"隧道类型",desc:"将 Gateway 暴露到公网的隧道服务",default:"none",options:["none","cloudflare","tailscale","ngrok","custom"]}}},identity:{label:"身份格式",defaultOpen:!1,fields:{"identity.format":{type:"enum",label:"身份格式",desc:"OpenClaw 或 AIEOS 身份文档格式",default:"openclaw",options:["openclaw","aieos"]}}}};function g(_){return _!==null&&typeof _=="object"&&!Array.isArray(_)}function $(_){return typeof _=="boolean"?"bool":typeof _=="number"?"number":Array.isArray(_)?"array":g(_)?"object":"string"}function C(_){const S=String(_).toLowerCase();return["key","token","secret","password","auth","credential","private"].some(P=>S.includes(P))}function D(_){return String(_).replace(/_/g," ").replace(/\b\w/g,S=>S.toUpperCase())}function ue(){const _=new Set;for(const S of Object.values(E))for(const P of Object.keys(S.fields))_.add(P.split(".")[0]);return _}const pe=ue();function Ke(_){if(!a(c))return[];const S=new Set(Object.keys(_.fields)),P=new Set;for(const re of Object.keys(_.fields))P.add(re.split(".")[0]);const U=[];for(const re of P){const ae=a(c)[re];if(g(ae))for(const[ge,xe]of Object.entries(ae)){const Ee=`${re}.${ge}`;S.has(Ee)||U.push({path:Ee,key:ge,value:xe})}}return U}const ze=ne(()=>a(c)?Object.keys(a(c)).filter(_=>!pe.has(_)).sort():[]),W=Object.entries(E),Q=ne(()=>[...W.map(([_,S])=>({groupKey:_,label:S.label,dynamic:!1})),...a(ze).map(_=>({groupKey:_,label:D(_),dynamic:!0}))]);function ve(_,S){if(!_)return;const P=S.split(".");let U=_;for(const re of P){if(U==null||typeof U!="object")return;U=U[re]}return U}function se(_,S,P){const U=S.split(".");let re=_;for(let ae=0;ae<U.length-1;ae++)(re[U[ae]]==null||typeof re[U[ae]]!="object")&&(re[U[ae]]={}),re=re[U[ae]];re[U[U.length-1]]=P}function Oe(_){if(a(c))return ve(a(c),_)}function z(_){return JSON.parse(JSON.stringify(_))}function V(_,S){return JSON.stringify(_)===JSON.stringify(S)}function me(_,S,P){const U=[],re=new Set([...Object.keys(_||{}),...Object.keys(S||{})]);for(const ae of re){const ge=P?`${P}.${ae}`:ae,xe=(_||{})[ae],Ee=(S||{})[ae];g(xe)&&g(Ee)?U.push(...me(xe,Ee,ge)):V(xe,Ee)||U.push({fieldPath:ge,newVal:xe,oldVal:Ee})}return U}function Fe(){return!a(c)||!a(u)?[]:me(a(c),a(u),"").map(S=>{for(const U of Object.values(E))if(U.fields[S.fieldPath])return{...S,label:U.fields[S.fieldPath].label,group:U.label};const P=S.fieldPath.split(".");return{...S,label:D(P[P.length-1]),group:D(P[0])}})}const oe=ne(()=>!!(a(c)&&a(u)&&JSON.stringify(a(c))!==JSON.stringify(a(u)))),X=ne(Fe),le=ne(()=>new Set(a(X).map(_=>_.fieldPath)));function Re(_){for(const S of a(le))if(S===_||S.startsWith(_+"."))return!0;return!1}function Je(_){return`config-section-${_}`}function ot(_){if(b(Z,_,!0),typeof document>"u")return;const S=document.getElementById(Je(_));S&&(S instanceof HTMLDetailsElement&&(S.open=!0),S.scrollIntoView({behavior:"smooth",block:"start"}),typeof history<"u"&&history.replaceState(null,"",`#${Je(_)}`))}function tt(){if(typeof window>"u")return;const _=window.location.hash.replace(/^#/,"");if(!_.startsWith("config-section-"))return;const S=_.replace(/^config-section-/,"");a(Q).some(P=>P.groupKey===S)&&ot(S)}const ct=ne(()=>a(c)?JSON.stringify(a(c),null,2):""),R=ne(()=>Ic(a(ct)));function H(_,S){if(!a(c))return;const P=z(a(c));se(P,_,S),b(c,P,!0)}function ye(_){const S=Oe(_),P=Array.isArray(S)?[...S,""]:[""];H(_,P)}function it(_,S){const P=Oe(_);Array.isArray(P)&&H(_,P.filter((U,re)=>re!==S))}function rt(_,S,P){const U=Oe(_);if(!Array.isArray(U))return;const re=[...U];re[S]=P,H(_,re)}function st(_){const S=new Set(a(O));S.has(_)?S.delete(_):S.add(_),b(O,S,!0)}function xt(_){return _==null?"null":typeof _=="boolean"?_?"true":"false":Array.isArray(_)||typeof _=="object"?JSON.stringify(_):String(_)}function je(_){return!!(g(_)||Array.isArray(_)&&_.some(S=>g(S)||Array.isArray(S)))}async function Ye(){try{const[_,S]=await Promise.all([$t.getConfig(),$t.getStatus().catch(()=>null)]);b(c,typeof _=="object"&&_?_:{},!0),b(u,z(a(c)),!0),b(y,S,!0),b(I,"")}catch(_){b(I,_ instanceof Error?_.message:"Failed to load config",!0)}finally{b(m,!1)}}async function vt(){if(!(!a(oe)||a(k))){b(k,!0),b(T,"");try{const _={};for(const P of a(X))se(_,P.fieldPath,P.newVal);const S=await $t.saveConfig(_);b(u,z(a(c)),!0),b(M,!1),S!=null&&S.restart_required?b(T,"已保存，部分设置需要重启服务后生效"):b(T,"已保存"),setTimeout(()=>{b(T,"")},5e3)}catch(_){b(T,"保存失败: "+(_ instanceof Error?_.message:String(_)))}finally{b(k,!1)}}}function Ue(){a(u)&&(b(c,z(a(u)),!0),b(M,!1))}async function wt(){if(!(!a(ct)||typeof navigator>"u"||!navigator.clipboard))try{await navigator.clipboard.writeText(a(ct))}catch{}}Bt(()=>{Ye()}),Bt(()=>{a(m)||a(F)||a(Q).length===0||queueMicrotask(()=>{tt()})});var fe=Su(),Ne=i(fe),he=i(Ne),Te=i(he),j=v(he,2),G=i(j),qe=i(G),at=v(G,2),ut=v(Ne,2);{var St=_=>{var S=iu();f(_,S)},w=_=>{var S=lu(),P=i(S);N(()=>p(P,a(I))),f(_,S)},q=_=>{var S=du(),P=i(S),U=i(P),re=i(U);fl(re,()=>a(R)),f(_,S)},ee=_=>{var S=mu(),P=i(S),U=i(P);nt(U,21,()=>a(Q),dt,(xe,Ee)=>{const lt=ne(()=>Re(a(Ee).groupKey));var _e=fu(),ie=i(_e),Ie=i(ie),$e=v(ie,2);{var Y=De=>{var ke=cu();f(De,ke)};B($e,De=>{a(Ee).dynamic&&De(Y)})}var te=v($e,2);{var be=De=>{var ke=uu();f(De,ke)};B(te,De=>{a(lt)&&De(be)})}N(()=>{et(_e,1,`inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-sm transition ${a(Z)===a(Ee).groupKey?"border-sky-500 bg-sky-500/10 text-sky-700 dark:text-sky-300":"border-gray-300 bg-white text-gray-600 hover:border-sky-400 hover:text-sky-600 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:border-sky-500 dark:hover:text-sky-300"}`),p(Ie,a(Ee).label)}),J("click",_e,()=>ot(a(Ee).groupKey)),f(xe,_e)});var re=v(P,2);nt(re,17,()=>W,dt,(xe,Ee)=>{var lt=ne(()=>La(a(Ee),2));let _e=()=>a(lt)[0],ie=()=>a(lt)[1];const Ie=ne(()=>K[_e()]),$e=ne(()=>Ke(ie())),Y=ne(()=>Object.keys(ie().fields)),te=ne(()=>a(Y).some(Ge=>a(le).has(Ge))||a($e).some(Ge=>Re(Ge.path)));var be=pu(),De=i(be),ke=i(De);{var ft=Ge=>{var Nt=Le(),Wt=Ae(Nt);vl(Wt,()=>a(Ie),(Nr,zt)=>{zt(Nr,{size:18,class:"text-gray-500 dark:text-gray-400"})}),f(Ge,Nt)};B(ke,Ge=>{a(Ie)&&Ge(ft)})}var ht=v(ke,2),Ct=i(ht),Mt=v(ht,2);{var We=Ge=>{var Nt=vu();f(Ge,Nt)};B(Mt,Ge=>{a(te)&&Ge(We)})}var ce=v(De,2),Xe=i(ce);nt(Xe,17,()=>Object.entries(ie().fields),dt,(Ge,Nt)=>{var Wt=ne(()=>La(a(Nt),2));r(Ge,()=>a(Wt)[0],()=>a(Wt)[1])});var gt=v(Xe,2);{var Ht=Ge=>{var Nt=gu(),Wt=v(i(Nt),2);nt(Wt,21,()=>a($e),dt,(Nr,zt)=>{let Ka=()=>a(zt).path,Na=()=>a(zt).key,sa=()=>a(zt).value;var oa=Le(),pn=Ae(oa);{var bn=ia=>{l(ia,Ka,Na,sa)},Po=ne(()=>g(sa())),Oo=ia=>{o(ia,Ka,Na,sa)};B(pn,ia=>{a(Po)?ia(bn):ia(Oo,-1)})}f(Nr,oa)}),f(Ge,Nt)};B(gt,Ge=>{a($e).length>0&&Ge(Ht)})}N(Ge=>{pt(be,"id",Ge),be.open=ie().defaultOpen,p(Ct,ie().label)},[()=>Je(_e())]),Er("toggle",be,Ge=>{Ge.currentTarget.open&&b(Z,_e(),!0)}),f(xe,be)});var ae=v(re,2);{var ge=xe=>{var Ee=hu(),lt=v(i(Ee),2);nt(lt,21,()=>a(ze),dt,(_e,ie)=>{const Ie=ne(()=>a(c)[a(ie)]),$e=ne(()=>Re(a(ie))),Y=ne(()=>$(a(Ie)));var te=yu(),be=i(te),De=i(be);Ad(De,{size:18,class:"flex-shrink-0 text-gray-400 dark:text-gray-500"});var ke=v(De,2),ft=i(ke),ht=v(ke,2);{var Ct=gt=>{var Ht=bu();f(gt,Ht)};B(ht,gt=>{a($e)&&gt(Ct)})}var Mt=v(ht,2),We=i(Mt),ce=v(be,2),Xe=i(ce);d(Xe,()=>a(ie),()=>a(Ie)),N(gt=>{pt(te,"id",gt),p(ft,a(ie)),p(We,a(Y))},[()=>Je(a(ie))]),Er("toggle",te,gt=>{gt.currentTarget.open&&b(Z,a(ie),!0)}),f(_e,te)}),f(xe,Ee)};B(ae,xe=>{a(ze).length>0&&xe(ge)})}f(_,S)};B(ut,_=>{a(m)?_(St):a(I)?_(w,1):a(F)?_(q,2):_(ee,-1)})}var de=v(ut,2);{var kt=_=>{var S=ku(),P=i(S),U=i(P),re=i(U),ae=i(re),ge=v(re,2),xe=i(ge),Ee=v(U,2),lt=i(Ee),_e=v(lt,2),ie=i(_e),Ie=v(P,2);{var $e=Y=>{var te=xu(),be=v(i(te),2);nt(be,21,()=>a(X),dt,(De,ke)=>{var ft=_u(),ht=i(ft),Ct=i(ht),Mt=v(ht,2),We=i(Mt),ce=v(Mt,2),Xe=i(ce),gt=v(ce,4),Ht=i(gt);N((Ge,Nt)=>{p(Ct,a(ke).group),p(We,a(ke).label),p(Xe,Ge),p(Ht,Nt)},[()=>xt(a(ke).oldVal),()=>xt(a(ke).newVal)]),f(De,ft)}),f(Y,te)};B(Ie,Y=>{a(M)&&Y($e)})}N(()=>{p(ae,`${a(X).length??""} 项更改`),p(xe,a(M)?"隐藏详情":"查看详情"),_e.disabled=a(k),p(ie,a(k)?"保存中...":"保存配置")}),J("click",ge,()=>b(M,!a(M))),J("click",lt,Ue),J("click",_e,vt),f(_,S)};B(de,_=>{a(oe)&&!a(m)&&!a(F)&&_(kt)})}var Ze=v(de,2);{var Be=_=>{var S=wu(),P=i(S);N(U=>{et(S,1,`fixed bottom-20 left-1/2 z-50 -translate-x-1/2 rounded-lg border px-4 py-2 text-sm shadow-lg ${U??""}`),p(P,a(T))},[()=>a(T).startsWith("保存失败")?"border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"]),f(_,S)};B(Ze,_=>{a(T)&&_(Be)})}N(_=>{p(Te,_),p(qe,a(F)?"结构化编辑":"JSON 视图")},[()=>h("config.title")]),J("click",G,()=>b(F,!a(F))),J("click",at,wt),f(e,fe),Me()}fr(["click","change","input"]);var Eu=x('<p class="text-gray-400 dark:text-gray-500"> </p>'),$u=x('<li class="whitespace-pre-wrap break-words"><span class="mr-3 select-none text-gray-400 dark:text-gray-600"> </span> <span> </span></li>'),Cu=x('<ol class="space-y-1"></ol>'),Mu=x('<section class="space-y-4"><div class="flex flex-wrap items-center justify-between gap-3"><h2 class="text-2xl font-semibold"> </h2> <div class="flex items-center gap-2"><span> </span> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></div> <div class="h-[65vh] overflow-y-auto rounded-xl border border-gray-200 bg-gray-50 p-4 font-mono text-xs leading-5 text-green-800 dark:border-gray-700 dark:bg-gray-950 dark:text-green-300"><!></div></section>');function Nu(e,t){Ce(t,!0);const r=1e3,n=500,s=1e4;let o=L(Et([])),l=L(!1),d=L("disconnected"),c=L(null),u=null,y=null,m=0,k=!0;const I=ne(()=>a(d)==="connected"?"border-green-500/50 bg-green-500/15 text-green-700 dark:text-green-300":a(d)==="reconnecting"?"border-amber-500/50 bg-amber-500/15 text-amber-700 dark:text-amber-200":"border-red-500/50 bg-red-500/15 text-red-700 dark:text-red-300"),T=ne(()=>a(d)==="connected"?h("logs.connected"):a(d)==="reconnecting"?h("logs.reconnecting"):h("logs.disconnected"));function F(oe){const X=rn?new URL(rn,window.location.href):new URL(window.location.href);return X.protocol=X.protocol==="https:"?"wss:":"ws:",X.pathname="/api/logs/stream",X.search=`token=${encodeURIComponent(oe)}`,X.hash="",X.toString()}function M(oe){if(typeof oe!="string"||oe.length===0)return;const X=oe.split(/\r?\n/).filter(Re=>Re.length>0);if(X.length===0)return;const le=[...a(o),...X];b(o,le.length>r?le.slice(le.length-r):le,!0)}function O(){y!==null&&(clearTimeout(y),y=null)}function Z(){u&&(u.onopen=null,u.onmessage=null,u.onerror=null,u.onclose=null,u.close(),u=null)}function K(){if(!k){b(d,"disconnected");return}b(d,"reconnecting");const oe=Math.min(n*2**m,s);m+=1,O(),y=setTimeout(()=>{y=null,E()},oe)}function E(){O();const oe=Ua();if(!oe){b(d,"disconnected");return}b(d,"reconnecting"),Z();let X;try{X=new WebSocket(F(oe))}catch{K();return}u=X,X.onopen=()=>{m=0,b(d,"connected")},X.onmessage=le=>{a(l)||M(le.data)},X.onerror=()=>{(X.readyState===WebSocket.OPEN||X.readyState===WebSocket.CONNECTING)&&X.close()},X.onclose=()=>{u=null,K()}}function g(){b(l,!a(l))}function $(){b(o,[],!0)}Bt(()=>(k=!0,E(),()=>{k=!1,O(),Z(),b(d,"disconnected")})),Bt(()=>{a(o).length,a(l),!(a(l)||!a(c))&&queueMicrotask(()=>{a(c)&&(a(c).scrollTop=a(c).scrollHeight)})});var C=Mu(),D=i(C),ue=i(D),pe=i(ue),Ke=v(ue,2),ze=i(Ke),W=i(ze),Q=v(ze,2),ve=i(Q),se=v(Q,2),Oe=i(se),z=v(D,2),V=i(z);{var me=oe=>{var X=Eu(),le=i(X);N(Re=>p(le,Re),[()=>h("logs.waiting")]),f(oe,X)},Fe=oe=>{var X=Cu();nt(X,21,()=>a(o),dt,(le,Re,Je)=>{var ot=$u(),tt=i(ot),ct=i(tt),R=v(tt,2),H=i(R);N(ye=>{p(ct,ye),p(H,a(Re))},[()=>String(Je+1).padStart(4,"0")]),f(le,ot)}),f(oe,X)};B(V,oe=>{a(o).length===0?oe(me):oe(Fe,-1)})}zn(z,oe=>b(c,oe),()=>a(c)),N((oe,X,le)=>{p(pe,oe),et(ze,1,`rounded-full border px-2 py-1 text-xs font-medium uppercase tracking-wide ${a(I)}`),p(W,a(T)),p(ve,X),p(Oe,le)},[()=>h("logs.title"),()=>a(l)?h("logs.resume"):h("logs.pause"),()=>h("logs.clear")]),J("click",Q,g),J("click",se,$),f(e,C),Me()}fr(["click"]);var Tu=x("<option> </option>"),Pu=x('<div class="rounded-xl border border-sky-500/30 bg-white p-4 space-y-3 dark:bg-gray-800"><h3 class="text-base font-semibold text-gray-900 dark:text-gray-100"> </h3> <div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2 pt-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),Ou=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Iu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Lu=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),Fu=x("<option> </option>"),Ru=x('<div class="space-y-3"><div class="grid gap-3 sm:grid-cols-2"><div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <select class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"></select></div> <div><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="number" min="1000" step="1000" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="sm:col-span-2"><label class="mb-1 block text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <input type="text" class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-900 dark:text-gray-200"/></div> <div class="flex items-center gap-2"><label class="text-xs font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </label> <button type="button"><span></span></button></div></div> <div class="flex justify-end gap-2"><button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div></div>'),ju=x('<div class="flex items-start justify-between gap-3"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-2 font-mono text-sm text-gray-500 dark:text-gray-400"> </p> <p class="mt-1 text-xs text-gray-400 dark:text-gray-500"> </p></div> <div class="flex items-center gap-2"><button type="button"><span></span></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-2 py-1 text-xs text-gray-600 hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-300 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-red-500/50 bg-red-500/10 px-2 py-1 text-xs text-red-600 hover:bg-red-500/20 dark:text-red-300"> </button></div></div>'),Du=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><!></article>'),Hu=x('<div class="space-y-3"></div>'),zu=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></div> <!> <!></section>');function Uu(e,t){Ce(t,!0);const r=["agent_start","agent_end","llm_request","llm_response","tool_call_start","tool_call_end","message_received","message_sent"];let n=L(Et([])),s=L(!0),o=L(""),l=L(null),d=L(!1),c=L(Et(r[0])),u=L(""),y=L(3e4),m=L(!0);function k(){b(c,r[0],!0),b(u,""),b(y,3e4),b(m,!0)}function I(z){return z.split("_").map(V=>V.charAt(0).toUpperCase()+V.slice(1)).join(" ")}async function T(){try{const z=await $t.getHooks();b(n,Array.isArray(z==null?void 0:z.hooks)?z.hooks:[],!0),b(o,"")}catch{b(n,[{id:"1",event:"message_received",command:'echo "msg received"',timeout_ms:3e4,enabled:!0},{id:"2",event:"agent_start",command:"/opt/scripts/on-start.sh",timeout_ms:1e4,enabled:!0},{id:"3",event:"tool_call_end",command:'notify-send "tool done"',timeout_ms:5e3,enabled:!1}],!0),b(o,"")}finally{b(s,!1)}}function F(z){b(l,z.id,!0),b(c,z.event,!0),b(u,z.command,!0),b(y,z.timeout_ms,!0),b(m,z.enabled,!0)}function M(){b(l,null),k()}function O(z){b(n,a(n).map(V=>V.id===z?{...V,event:a(c),command:a(u),timeout_ms:a(y),enabled:a(m)}:V),!0),b(l,null),k()}function Z(){if(!a(u).trim())return;const z={id:String(Date.now()),event:a(c),command:a(u).trim(),timeout_ms:a(y),enabled:a(m)};b(n,[...a(n),z],!0),b(d,!1),k()}function K(z){b(n,a(n).filter(V=>V.id!==z),!0)}function E(z){b(n,a(n).map(V=>V.id===z?{...V,enabled:!V.enabled}:V),!0)}Bt(()=>{T()});var g=zu(),$=i(g),C=i($),D=i(C),ue=v(C,2),pe=i(ue),Ke=v($,2);{var ze=z=>{var V=Pu(),me=i(V),Fe=i(me),oe=v(me,2),X=i(oe),le=i(X),Re=i(le),Je=v(le,2);nt(Je,21,()=>r,dt,(Te,j)=>{var G=Tu(),qe=i(G),at={};N(ut=>{p(qe,ut),at!==(at=a(j))&&(G.value=(G.__value=a(j))??"")},[()=>I(a(j))]),f(Te,G)});var ot=v(X,2),tt=i(ot),ct=i(tt),R=v(tt,2),H=v(ot,2),ye=i(H),it=i(ye),rt=v(ye,2),st=v(H,2),xt=i(st),je=i(xt),Ye=v(xt,2),vt=i(Ye),Ue=v(oe,2),wt=i(Ue),fe=i(wt),Ne=v(wt,2),he=i(Ne);N((Te,j,G,qe,at,ut,St,w)=>{p(Fe,Te),p(Re,j),p(ct,G),p(it,qe),pt(rt,"placeholder",at),p(je,ut),et(Ye,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(m)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(vt,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(m)?"translate-x-4":"translate-x-1"}`),p(fe,St),p(he,w)},[()=>h("hooks.newHook"),()=>h("hooks.event"),()=>h("hooks.timeout"),()=>h("hooks.command"),()=>h("hooks.commandPlaceholder"),()=>h("hooks.enabled"),()=>h("hooks.cancel"),()=>h("hooks.save")]),Hn(Je,()=>a(c),Te=>b(c,Te)),Kr(R,()=>a(y),Te=>b(y,Te)),Kr(rt,()=>a(u),Te=>b(u,Te)),J("click",Ye,()=>b(m,!a(m))),J("click",wt,()=>{b(d,!1),k()}),J("click",Ne,Z),f(z,V)};B(Ke,z=>{a(d)&&z(ze)})}var W=v(Ke,2);{var Q=z=>{var V=Ou(),me=i(V);N(Fe=>p(me,Fe),[()=>h("hooks.loading")]),f(z,V)},ve=z=>{var V=Iu(),me=i(V);N(()=>p(me,a(o))),f(z,V)},se=z=>{var V=Lu(),me=i(V);N(Fe=>p(me,Fe),[()=>h("hooks.noHooks")]),f(z,V)},Oe=z=>{var V=Hu();nt(V,21,()=>a(n),me=>me.id,(me,Fe)=>{var oe=Du(),X=i(oe);{var le=Je=>{var ot=Ru(),tt=i(ot),ct=i(tt),R=i(ct),H=i(R),ye=v(R,2);nt(ye,21,()=>r,dt,(St,w)=>{var q=Fu(),ee=i(q),de={};N(kt=>{p(ee,kt),de!==(de=a(w))&&(q.value=(q.__value=a(w))??"")},[()=>I(a(w))]),f(St,q)});var it=v(ct,2),rt=i(it),st=i(rt),xt=v(rt,2),je=v(it,2),Ye=i(je),vt=i(Ye),Ue=v(Ye,2),wt=v(je,2),fe=i(wt),Ne=i(fe),he=v(fe,2),Te=i(he),j=v(tt,2),G=i(j),qe=i(G),at=v(G,2),ut=i(at);N((St,w,q,ee,de,kt)=>{p(H,St),p(st,w),p(vt,q),p(Ne,ee),et(he,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(m)?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(Te,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(m)?"translate-x-4":"translate-x-1"}`),p(qe,de),p(ut,kt)},[()=>h("hooks.event"),()=>h("hooks.timeout"),()=>h("hooks.command"),()=>h("hooks.enabled"),()=>h("hooks.cancel"),()=>h("hooks.save")]),Hn(ye,()=>a(c),St=>b(c,St)),Kr(xt,()=>a(y),St=>b(y,St)),Kr(Ue,()=>a(u),St=>b(u,St)),J("click",he,()=>b(m,!a(m))),J("click",G,M),J("click",at,()=>O(a(Fe).id)),f(Je,ot)},Re=Je=>{var ot=ju(),tt=i(ot),ct=i(tt),R=i(ct),H=i(R),ye=v(R,2),it=i(ye),rt=v(ct,2),st=i(rt),xt=v(rt,2),je=i(xt),Ye=v(tt,2),vt=i(Ye),Ue=i(vt),wt=v(vt,2),fe=i(wt),Ne=v(wt,2),he=i(Ne);N((Te,j,G,qe,at)=>{p(H,Te),et(ye,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Fe).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(it,j),p(st,a(Fe).command),p(je,`${G??""}: ${a(Fe).timeout_ms??""}ms`),et(vt,1,`relative inline-flex h-5 w-9 items-center rounded-full transition ${a(Fe).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(Ue,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(Fe).enabled?"translate-x-4":"translate-x-1"}`),p(fe,qe),p(he,at)},[()=>I(a(Fe).event),()=>a(Fe).enabled?h("common.enabled"):h("common.disabled"),()=>h("hooks.timeout"),()=>h("hooks.edit"),()=>h("hooks.delete")]),J("click",vt,()=>E(a(Fe).id)),J("click",wt,()=>F(a(Fe))),J("click",Ne,()=>K(a(Fe).id)),f(Je,ot)};B(X,Je=>{a(l)===a(Fe).id?Je(le):Je(Re,-1)})}f(me,oe)}),f(z,V)};B(W,z=>{a(s)?z(Q):a(o)?z(ve,1):a(n).length===0?z(se,2):z(Oe,-1)})}N((z,V)=>{p(D,z),p(pe,V)},[()=>h("hooks.title"),()=>a(d)?h("hooks.cancelAdd"):h("hooks.addHook")]),J("click",ue,()=>{b(d,!a(d)),a(d)&&k()}),f(e,g),Me()}fr(["click"]);var Bu=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),Wu=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),Vu=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),qu=x('<p class="mt-1 text-xs text-gray-500 dark:text-gray-400"> </p>'),Gu=x('<div class="rounded-lg border border-gray-200 bg-gray-50/60 p-3 dark:border-gray-700 dark:bg-gray-900/60"><p class="font-mono text-sm font-medium text-gray-700 dark:text-gray-200"> </p> <!></div>'),Ku=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><h4 class="mb-3 text-sm font-medium uppercase tracking-wide text-gray-500 dark:text-gray-400"> </h4> <div class="grid gap-2"></div></div>'),Ju=x('<div class="border-t border-gray-200 p-4 dark:border-gray-700"><p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),Yu=x('<article class="rounded-xl border border-gray-200 bg-white dark:border-gray-700 dark:bg-gray-800"><button type="button" class="flex w-full items-center justify-between gap-3 p-4 text-left"><div class="min-w-0 flex-1"><div class="flex items-center gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <span> </span></div> <p class="mt-1 font-mono text-sm text-gray-500 dark:text-gray-400"> </p></div> <span class="text-xs text-gray-400 dark:text-gray-500"> </span></button> <!></article>'),Xu=x('<div class="space-y-4"></div>'),Qu=x('<section class="space-y-6"><div class="flex items-center justify-between"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>');function Zu(e,t){Ce(t,!0);let r=L(Et([])),n=L(!0),s=L(""),o=L(null);async function l(){try{const E=await $t.getMcpServers();b(r,Array.isArray(E==null?void 0:E.servers)?E.servers:[],!0),b(s,"")}catch{b(r,[{name:"filesystem",url:"stdio:///usr/local/bin/mcp-filesystem",status:"connected",tools:[{name:"read_file",description:"Read contents of a file"},{name:"write_file",description:"Write content to a file"},{name:"list_directory",description:"List directory contents"}]},{name:"github",url:"https://mcp.github.com/sse",status:"connected",tools:[{name:"search_repositories",description:"Search GitHub repositories"},{name:"create_issue",description:"Create a new issue"},{name:"list_pull_requests",description:"List pull requests"}]},{name:"database",url:"stdio:///opt/mcp/db-server",status:"disconnected",tools:[]}],!0),b(s,"")}finally{b(n,!1)}}function d(E){b(o,a(o)===E?null:E,!0)}async function c(){b(n,!0),await l()}Bt(()=>{l()});var u=Qu(),y=i(u),m=i(y),k=i(m),I=v(m,2),T=i(I),F=v(y,2);{var M=E=>{var g=Bu(),$=i(g);N(C=>p($,C),[()=>h("mcp.loading")]),f(E,g)},O=E=>{var g=Wu(),$=i(g);N(()=>p($,a(s))),f(E,g)},Z=E=>{var g=Vu(),$=i(g);N(C=>p($,C),[()=>h("mcp.noServers")]),f(E,g)},K=E=>{var g=Xu();nt(g,21,()=>a(r),dt,($,C)=>{var D=Yu(),ue=i(D),pe=i(ue),Ke=i(pe),ze=i(Ke),W=i(ze),Q=v(ze,2),ve=i(Q),se=v(Ke,2),Oe=i(se),z=v(pe,2),V=i(z),me=v(ue,2);{var Fe=X=>{var le=Ku(),Re=i(le),Je=i(Re),ot=v(Re,2);nt(ot,21,()=>a(C).tools,dt,(tt,ct)=>{var R=Gu(),H=i(R),ye=i(H),it=v(H,2);{var rt=st=>{var xt=qu(),je=i(xt);N(()=>p(je,a(ct).description)),f(st,xt)};B(it,st=>{a(ct).description&&st(rt)})}N(()=>p(ye,a(ct).name)),f(tt,R)}),N(tt=>p(Je,tt),[()=>h("mcp.availableTools")]),f(X,le)},oe=X=>{var le=Ju(),Re=i(le),Je=i(Re);N(ot=>p(Je,ot),[()=>h("mcp.noTools")]),f(X,le)};B(me,X=>{a(o)===a(C).name&&a(C).tools&&a(C).tools.length>0?X(Fe):a(o)===a(C).name&&(!a(C).tools||a(C).tools.length===0)&&X(oe,1)})}N((X,le)=>{var Re;p(W,a(C).name),et(Q,1,`rounded-full px-2 py-1 text-xs font-medium ${a(C).status==="connected"?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(ve,X),p(Oe,a(C).url),p(V,`${((Re=a(C).tools)==null?void 0:Re.length)??0??""} ${le??""}`)},[()=>a(C).status==="connected"?h("mcp.connected"):h("mcp.disconnected"),()=>h("mcp.tools")]),J("click",ue,()=>d(a(C).name)),f($,D)}),f(E,g)};B(F,E=>{a(n)?E(M):a(s)?E(O,1):a(r).length===0?E(Z,2):E(K,-1)})}N((E,g)=>{p(k,E),p(T,g)},[()=>h("mcp.title"),()=>h("common.refresh")]),J("click",I,c),f(e,u),Me()}fr(["click"]);var e0=x('<span class="text-sm text-gray-500 dark:text-gray-400"> </span>'),t0=x("<div> </div>"),r0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),a0=x('<p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300"> </p>'),n0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),s0=x('<p class="mt-2 text-sm text-gray-500 dark:text-gray-400"> </p>'),o0=x('<div class="flex items-center gap-2"><span class="text-xs text-yellow-600 dark:text-yellow-400"> </span> <button type="button" class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"> </button> <button type="button" class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"> </button></div>'),i0=x('<button type="button" class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"> </button>'),l0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-3"><h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <button type="button"><span></span></button></div> <!> <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500"> </p> <div class="mt-3 flex items-center justify-between"><span> </span> <!></div></article>'),d0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),c0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),u0=x('<p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300"> </p>'),f0=x('<p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400"> </p>'),v0=x('<span class="flex items-center gap-1"><svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"></path></svg> </span>'),g0=x('<span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700"> </span>'),p0=x('<span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300"> </span>'),b0=x('<button type="button" class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button>'),y0=x('<article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="flex items-start justify-between gap-2"><div class="min-w-0 flex-1"><h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-400 dark:text-gray-500"> </p></div> <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300"> </span></div> <!> <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500"><!> <!> <span> </span></div> <div class="mt-3 flex items-center justify-between"><a target="_blank" rel="noopener noreferrer" class="text-xs text-sky-600 hover:underline dark:text-sky-400"> </a> <!></div></article>'),h0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),m0=x('<div class="flex flex-col gap-3 sm:flex-row"><select class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"><option>GitHub</option><option>ClawHub</option><option>HuggingFace</option></select> <input type="text" class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"/> <button type="button" class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"> </button></div> <!>',1),_0=x('<section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-3"><h2 class="text-2xl font-semibold"> </h2> <!></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50"><button type="button"> </button> <button type="button"> </button></div> <!> <!> <!></section>');function x0(e,t){Ce(t,!0);let r=L("installed"),n=L(Et([])),s=L(!0),o=L(""),l=L(""),d=L("success"),c=L(Et([])),u=L(!1),y=L(""),m=L("github"),k=L(!1),I=L(""),T=L(""),F=L("");function M(R,H="success"){b(l,R,!0),b(d,H,!0),setTimeout(()=>{b(l,"")},3e3)}async function O(){try{const R=await $t.getSkills();b(n,Array.isArray(R==null?void 0:R.skills)?R.skills:[],!0),b(o,"")}catch{b(n,[],!0),b(o,"Failed to load skills.")}finally{b(s,!1)}}async function Z(R){try{await $t.toggleSkill(R),b(n,a(n).map(H=>H.name===R?{...H,enabled:!H.enabled}:H),!0)}catch{b(n,a(n).map(H=>H.name===R?{...H,enabled:!H.enabled}:H),!0)}}async function K(R){if(a(F)!==R){b(F,R,!0);return}b(F,""),b(T,R,!0);try{await $t.uninstallSkill(R),b(n,a(n).filter(H=>H.name!==R),!0),M(h("skills.uninstallSuccess"))}catch(H){M(h("skills.uninstallFailed")+(H.message?`: ${H.message}`:""),"error")}finally{b(T,"")}}const E=ne(()=>[...a(n)].sort((R,H)=>R.enabled===H.enabled?0:R.enabled?-1:1)),g=ne(()=>a(n).filter(R=>R.enabled).length);async function $(){!a(y).trim()&&a(m)==="github"&&b(y,"agent skill"),b(u,!0),b(k,!0);try{const R=await $t.discoverSkills(a(m),a(y));b(c,Array.isArray(R==null?void 0:R.results)?R.results:[],!0)}catch{b(c,[],!0)}finally{b(u,!1)}}function C(R){return a(n).some(H=>H.name===R)}async function D(R,H){b(I,R,!0);try{const ye=await $t.installSkill(R,H);ye!=null&&ye.skill&&b(n,[...a(n),{...ye.skill,enabled:!0}],!0),M(h("skills.installSuccess"))}catch(ye){M(h("skills.installFailed")+(ye.message?`: ${ye.message}`:""),"error")}finally{b(I,"")}}function ue(R){R.key==="Enter"&&$()}Bt(()=>{O()});var pe=_0(),Ke=i(pe),ze=i(Ke),W=i(ze),Q=i(W),ve=v(W,2);{var se=R=>{var H=e0(),ye=i(H);N(it=>p(ye,`${a(g)??""}/${a(n).length??""} ${it??""}`),[()=>h("skills.active")]),f(R,H)};B(ve,R=>{!a(s)&&a(n).length>0&&R(se)})}var Oe=v(ze,2),z=i(Oe),V=v(Ke,2),me=i(V),Fe=i(me),oe=v(me,2),X=i(oe),le=v(V,2);{var Re=R=>{var H=t0(),ye=i(H);N(()=>{et(H,1,`rounded-lg px-4 py-2 text-sm ${a(d)==="error"?"border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300":"border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300"}`),p(ye,a(l))}),f(R,H)};B(le,R=>{a(l)&&R(Re)})}var Je=v(le,2);{var ot=R=>{var H=Le(),ye=Ae(H);{var it=je=>{var Ye=r0(),vt=i(Ye);N(Ue=>p(vt,Ue),[()=>h("skills.loading")]),f(je,Ye)},rt=je=>{var Ye=a0(),vt=i(Ye);N(()=>p(vt,a(o))),f(je,Ye)},st=je=>{var Ye=n0(),vt=i(Ye);N(Ue=>p(vt,Ue),[()=>h("skills.noSkills")]),f(je,Ye)},xt=je=>{var Ye=d0();nt(Ye,21,()=>a(E),dt,(vt,Ue)=>{var wt=l0(),fe=i(wt),Ne=i(fe),he=i(Ne),Te=v(Ne,2),j=i(Te),G=v(fe,2);{var qe=Ze=>{var Be=s0(),_=i(Be);N(()=>p(_,a(Ue).description)),f(Ze,Be)};B(G,Ze=>{a(Ue).description&&Ze(qe)})}var at=v(G,2),ut=i(at),St=v(at,2),w=i(St),q=i(w),ee=v(w,2);{var de=Ze=>{var Be=o0(),_=i(Be),S=i(_),P=v(_,2),U=i(P),re=v(P,2),ae=i(re);N((ge,xe,Ee)=>{p(S,ge),P.disabled=a(T)===a(Ue).name,p(U,xe),p(ae,Ee)},[()=>h("skills.confirmUninstall").replace("{name}",a(Ue).name),()=>a(T)===a(Ue).name?h("skills.uninstalling"):h("common.yes"),()=>h("common.no")]),J("click",P,()=>K(a(Ue).name)),J("click",re,()=>{b(F,"")}),f(Ze,Be)},kt=Ze=>{var Be=i0(),_=i(Be);N(S=>p(_,S),[()=>h("skills.uninstall")]),J("click",Be,()=>K(a(Ue).name)),f(Ze,Be)};B(ee,Ze=>{a(F)===a(Ue).name?Ze(de):Ze(kt,-1)})}N(Ze=>{p(he,a(Ue).name),et(Te,1,`relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition ${a(Ue).enabled?"bg-sky-600":"bg-gray-400 dark:bg-gray-600"}`),et(j,1,`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${a(Ue).enabled?"translate-x-4":"translate-x-1"}`),p(ut,a(Ue).location),et(w,1,`rounded-full px-2 py-1 text-xs font-medium ${a(Ue).enabled?"border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300":"border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300"}`),p(q,Ze)},[()=>a(Ue).enabled?h("common.enabled"):h("common.disabled")]),J("click",Te,()=>Z(a(Ue).name)),f(vt,wt)}),f(je,Ye)};B(ye,je=>{a(s)?je(it):a(o)?je(rt,1):a(n).length===0?je(st,2):je(xt,-1)})}f(R,H)};B(Je,R=>{a(r)==="installed"&&R(ot)})}var tt=v(Je,2);{var ct=R=>{var H=m0(),ye=Ae(H),it=i(ye),rt=i(it);rt.value=rt.__value="github";var st=v(rt);st.value=st.__value="clawhub";var xt=v(st);xt.value=xt.__value="huggingface";var je=v(it,2),Ye=v(je,2),vt=i(Ye),Ue=v(ye,2);{var wt=he=>{var Te=c0(),j=i(Te);N(G=>p(j,G),[()=>h("skills.searching")]),f(he,Te)},fe=he=>{var Te=u0(),j=i(Te);N(G=>p(j,G),[()=>h("skills.noResults")]),f(he,Te)},Ne=he=>{var Te=h0();nt(Te,21,()=>a(c),dt,(j,G)=>{const qe=ne(()=>C(a(G).name));var at=y0(),ut=i(at),St=i(ut),w=i(St),q=i(w),ee=v(w,2),de=i(ee),kt=v(St,2),Ze=i(kt),Be=v(ut,2);{var _=Y=>{var te=f0(),be=i(te);N(()=>p(be,a(G).description)),f(Y,te)};B(Be,Y=>{a(G).description&&Y(_)})}var S=v(Be,2),P=i(S);{var U=Y=>{var te=v0(),be=v(i(te));N(()=>p(be,` ${a(G).stars??""}`)),f(Y,te)};B(P,Y=>{a(G).stars>0&&Y(U)})}var re=v(P,2);{var ae=Y=>{var te=g0(),be=i(te);N(()=>p(be,a(G).language)),f(Y,te)};B(re,Y=>{a(G).language&&Y(ae)})}var ge=v(re,2),xe=i(ge),Ee=v(S,2),lt=i(Ee),_e=i(lt),ie=v(lt,2);{var Ie=Y=>{var te=p0(),be=i(te);N(De=>p(be,De),[()=>h("skills.installed")]),f(Y,te)},$e=Y=>{var te=b0(),be=i(te);N(De=>{te.disabled=a(I)===a(G).url,p(be,De)},[()=>a(I)===a(G).url?h("skills.installing"):h("skills.install")]),J("click",te,()=>D(a(G).url,a(G).name)),f(Y,te)};B(ie,Y=>{a(qe)?Y(Ie):Y($e,-1)})}N((Y,te,be)=>{p(q,a(G).name),p(de,`${Y??""} ${a(G).owner??""}`),p(Ze,a(G).source),et(ge,1,a(G).has_license?"text-green-600 dark:text-green-400":"text-yellow-600 dark:text-yellow-400"),p(xe,te),pt(lt,"href",a(G).url),p(_e,be)},[()=>h("skills.owner"),()=>a(G).has_license?h("skills.licensed"):h("skills.unlicensed"),()=>a(G).url.replace("https://github.com/","")]),f(j,at)}),f(he,Te)};B(Ue,he=>{a(u)?he(wt):a(k)&&a(c).length===0?he(fe,1):a(c).length>0&&he(Ne,2)})}N((he,Te)=>{pt(je,"placeholder",he),Ye.disabled=a(u),p(vt,Te)},[()=>h("skills.search"),()=>a(u)?h("skills.searching"):h("skills.searchBtn")]),Hn(it,()=>a(m),he=>b(m,he)),J("keydown",je,ue),Kr(je,()=>a(y),he=>b(y,he)),J("click",Ye,$),f(R,H)};B(tt,R=>{a(r)==="discover"&&R(ct)})}N((R,H,ye,it)=>{p(Q,R),p(z,H),et(me,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="installed"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),p(Fe,ye),et(oe,1,`rounded-md px-4 py-2 text-sm font-medium transition ${a(r)==="discover"?"bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white":"text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200"}`),p(X,it)},[()=>h("skills.title"),()=>h("common.refresh"),()=>h("skills.tabInstalled"),()=>h("skills.tabDiscover")]),J("click",Oe,()=>{b(s,!0),O()}),J("click",me,()=>{b(r,"installed")}),J("click",oe,()=>{b(r,"discover")}),f(e,pe),Me()}fr(["click","keydown"]);var k0=x("<div> </div>"),w0=x('<p class="text-sm text-gray-500 dark:text-gray-400"> </p>'),S0=x('<div class="rounded-lg border border-red-200 bg-red-50 p-4 text-sm text-red-600 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400"> </div>'),A0=x('<div class="rounded-lg border border-gray-200 bg-gray-50 p-8 text-center dark:border-gray-700 dark:bg-gray-800"><!> <p class="text-sm text-gray-500 dark:text-gray-400"> </p></div>'),E0=x('<p class="mb-3 text-sm text-gray-600 dark:text-gray-300"> </p>'),$0=x('<span class="rounded-full bg-sky-100 px-2 py-0.5 text-xs text-sky-700 dark:bg-sky-900/30 dark:text-sky-300"> </span>'),C0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),M0=x('<span class="rounded-full bg-amber-100 px-2 py-0.5 text-xs text-amber-700 dark:bg-amber-900/30 dark:text-amber-300"> </span>'),N0=x('<div class="mb-3"><p class="mb-1 text-xs font-medium text-gray-500 dark:text-gray-400"> </p> <div class="flex flex-wrap gap-1"></div></div>'),T0=x('<div class="rounded-lg border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800"><div class="mb-3 flex items-start justify-between"><div><h3 class="font-semibold text-gray-900 dark:text-gray-100"> </h3> <p class="text-xs text-gray-500 dark:text-gray-400"> </p></div> <div><!> <span class="text-xs"> </span></div></div> <!> <!> <!> <div class="flex justify-end"><button type="button" class="flex items-center gap-1 rounded-lg border border-gray-300 bg-white px-3 py-1.5 text-xs text-gray-700 transition hover:bg-gray-100 disabled:opacity-50 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-200 dark:hover:bg-gray-600"><!> </button></div></div>'),P0=x('<div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3"></div>'),O0=x('<!> <section class="space-y-6"><div class="flex items-center justify-between"><div class="flex items-center gap-2"><!> <h2 class="text-2xl font-semibold"> </h2></div> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div> <!></section>',1);function I0(e,t){Ce(t,!0);let r=L(Et([])),n=L(!0),s=L(""),o=L(""),l=L(""),d=L("success");function c(W,Q="success"){b(l,W,!0),b(d,Q,!0),setTimeout(()=>{b(l,"")},3e3)}async function u(){b(n,!0);try{const W=await $t.getPlugins();b(r,Array.isArray(W==null?void 0:W.plugins)?W.plugins:[],!0),b(s,"")}catch{b(r,[],!0),b(s,h("plugins.loadFailed"),!0)}finally{b(n,!1)}}async function y(W){b(o,W,!0);try{await $t.reloadPlugin(W),c(h("plugins.reloadSuccess",{name:W})),await u()}catch(Q){c(h("plugins.reloadFailed")+(Q.message?`: ${Q.message}`:""),"error")}finally{b(o,"")}}function m(W){return typeof W=="string"&&W==="Active"?"text-green-500":typeof W=="object"&&(W!=null&&W.Error)?"text-red-500":"text-yellow-500"}function k(W){return typeof W=="string"&&W==="Active"?h("plugins.statusActive"):typeof W=="object"&&(W!=null&&W.Error)?W.Error:h("common.unknown")}Bt(()=>{u()});var I=O0(),T=Ae(I);{var F=W=>{var Q=k0(),ve=i(Q);N(()=>{et(Q,1,`fixed right-4 top-4 z-50 rounded-lg px-4 py-2 text-sm font-medium text-white shadow-lg transition ${a(d)==="error"?"bg-red-600":"bg-green-600"}`),p(ve,a(l))}),f(W,Q)};B(T,W=>{a(l)&&W(F)})}var M=v(T,2),O=i(M),Z=i(O),K=i(Z);ks(K,{size:24});var E=v(K,2),g=i(E),$=v(Z,2),C=i($),D=v(O,2);{var ue=W=>{var Q=w0(),ve=i(Q);N(se=>p(ve,se),[()=>h("plugins.loading")]),f(W,Q)},pe=W=>{var Q=S0(),ve=i(Q);N(()=>p(ve,a(s))),f(W,Q)},Ke=W=>{var Q=A0(),ve=i(Q);ks(ve,{size:40,class:"mx-auto mb-3 text-gray-400 dark:text-gray-500"});var se=v(ve,2),Oe=i(se);N(z=>p(Oe,z),[()=>h("plugins.noPlugins")]),f(W,Q)},ze=W=>{var Q=P0();nt(Q,21,()=>a(r),dt,(ve,se)=>{var Oe=T0(),z=i(Oe),V=i(z),me=i(V),Fe=i(me),oe=v(me,2),X=i(oe),le=v(V,2),Re=i(le);{var Je=fe=>{kd(fe,{size:16})},ot=fe=>{xd(fe,{size:16})};B(Re,fe=>{typeof a(se).status=="string"&&a(se).status==="Active"?fe(Je):fe(ot,-1)})}var tt=v(Re,2),ct=i(tt),R=v(z,2);{var H=fe=>{var Ne=E0(),he=i(Ne);N(()=>p(he,a(se).description)),f(fe,Ne)};B(R,fe=>{a(se).description&&fe(H)})}var ye=v(R,2);{var it=fe=>{var Ne=C0(),he=i(Ne),Te=i(he),j=v(he,2);nt(j,21,()=>a(se).capabilities,dt,(G,qe)=>{var at=$0(),ut=i(at);N(()=>p(ut,a(qe))),f(G,at)}),N(G=>p(Te,G),[()=>h("plugins.capabilities")]),f(fe,Ne)};B(ye,fe=>{var Ne;(Ne=a(se).capabilities)!=null&&Ne.length&&fe(it)})}var rt=v(ye,2);{var st=fe=>{var Ne=N0(),he=i(Ne),Te=i(he),j=v(he,2);nt(j,21,()=>a(se).permissions_required,dt,(G,qe)=>{var at=M0(),ut=i(at);N(()=>p(ut,a(qe))),f(G,at)}),N(G=>p(Te,G),[()=>h("plugins.permissions")]),f(fe,Ne)};B(rt,fe=>{var Ne;(Ne=a(se).permissions_required)!=null&&Ne.length&&fe(st)})}var xt=v(rt,2),je=i(xt),Ye=i(je);{var vt=fe=>{Nd(fe,{size:14,class:"animate-spin"})},Ue=fe=>{To(fe,{size:14})};B(Ye,fe=>{a(o)===a(se).name?fe(vt):fe(Ue,-1)})}var wt=v(Ye);N((fe,Ne,he)=>{p(Fe,a(se).name),p(X,`v${a(se).version??""}`),et(le,1,`flex items-center gap-1 ${fe??""}`),p(ct,Ne),je.disabled=a(o)===a(se).name,p(wt,` ${he??""}`)},[()=>m(a(se).status),()=>k(a(se).status),()=>h("plugins.reload")]),J("click",je,()=>y(a(se).name)),f(ve,Oe)}),f(W,Q)};B(D,W=>{a(n)?W(ue):a(s)?W(pe,1):a(r).length===0?W(Ke,2):W(ze,-1)})}N((W,Q)=>{p(g,W),p(C,Q)},[()=>h("plugins.title"),()=>h("common.refresh")]),J("click",$,u),f(e,I),Me()}fr(["click"]);var L0=x('<button type="button" class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"></button>'),F0=x('<button type="button"> </button>'),R0=x('<section class="space-y-4"><h2 class="text-2xl font-semibold"> </h2> <button type="button" class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"> </button></section>'),j0=x('<div class="flex min-h-screen"><!> <aside><div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700"><p class="text-lg font-semibold"> </p></div> <nav class="space-y-1"></nav></aside> <div class="flex min-w-0 flex-1 flex-col"><header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95"><div class="flex items-center gap-3"><button type="button" class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"> </button> <h1 class="text-lg font-semibold"> </h1></div> <div class="flex items-center gap-2"><button type="button" aria-label="Toggle theme" class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"><!></button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button> <button type="button" class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"> </button></div></header> <main class="flex-1 p-4 sm:p-6"><!></main></div></div>'),D0=x('<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100"><!></div>');function H0(e,t){Ce(t,!0);let r=L(Et(No())),n=L(Et(Ua())),s=L(!1),o=L(!0);const l=ne(()=>a(n).length>0),d=ne(()=>a(l)&&a(r)==="/"?"/overview":a(r)),c=ne(()=>a(d).startsWith("/chat/")?"/sessions":a(d));function u(C){try{return decodeURIComponent(C)}catch{return C}}const y=ne(()=>a(d).startsWith("/chat/")?u(a(d).slice(6)):"");function m(){localStorage.getItem("prx-console-theme")==="light"?b(o,!1):b(o,!0),k()}function k(){a(o)?document.documentElement.classList.add("dark"):document.documentElement.classList.remove("dark")}function I(){b(o,!a(o)),localStorage.setItem("prx-console-theme",a(o)?"dark":"light"),k()}function T(){b(n,Ua(),!0)}function F(C){b(r,C,!0),b(s,!1)}function M(C){b(n,C,!0),Pr("/overview",!0)}function O(){$o(),b(n,""),Pr("/",!0)}function Z(C){Pr(C)}Bt(()=>{m();const C=fd(F),D=ue=>{if(ue.key==="prx-console-token"){T();return}if(ue.key===gn&&ud(),ue.key==="prx-console-theme"){const pe=localStorage.getItem("prx-console-theme");b(o,pe!=="light"),k()}};return window.addEventListener("storage",D),()=>{C(),window.removeEventListener("storage",D)}}),Bt(()=>{if(a(l)&&a(r)==="/"){Pr("/overview",!0);return}!a(l)&&a(r)!=="/"&&Pr("/",!0)});var K=D0(),E=i(K);{var g=C=>{zd(C,{onLogin:M})},$=C=>{var D=j0(),ue=i(D);{var pe=j=>{var G=L0();N(qe=>pt(G,"aria-label",qe),[()=>h("app.closeSidebar")]),J("click",G,()=>b(s,!1)),f(j,G)};B(ue,j=>{a(s)&&j(pe)})}var Ke=v(ue,2),ze=i(Ke),W=i(ze),Q=i(W),ve=v(ze,2);nt(ve,21,()=>Tl,dt,(j,G)=>{var qe=F0(),at=i(qe);N(ut=>{et(qe,1,`w-full rounded-lg px-3 py-2 text-left text-sm transition ${a(c)===a(G).path?"bg-sky-600 text-white":"text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100"}`),p(at,ut)},[()=>h(a(G).labelKey)]),J("click",qe,()=>Z(a(G).path)),f(j,qe)});var se=v(Ke,2),Oe=i(se),z=i(Oe),V=i(z),me=i(V),Fe=v(V,2),oe=i(Fe),X=v(z,2),le=i(X),Re=i(le);{var Je=j=>{Rd(j,{size:16})},ot=j=>{Pd(j,{size:16})};B(Re,j=>{a(o)?j(Je):j(ot,-1)})}var tt=v(le,2),ct=i(tt),R=v(tt,2),H=i(R),ye=v(Oe,2),it=i(ye);{var rt=j=>{ec(j,{})},st=j=>{lc(j,{})},xt=j=>{Sc(j,{get sessionId(){return a(y)}})},je=ne(()=>a(d).startsWith("/chat/")),Ye=j=>{Pc(j,{})},vt=j=>{Uu(j,{})},Ue=j=>{Zu(j,{})},wt=j=>{x0(j,{})},fe=j=>{I0(j,{})},Ne=j=>{Au(j,{})},he=j=>{Nu(j,{})},Te=j=>{var G=R0(),qe=i(G),at=i(qe),ut=v(qe,2),St=i(ut);N((w,q)=>{p(at,w),p(St,q)},[()=>h("app.notFound"),()=>h("app.backToOverview")]),J("click",ut,()=>Z("/overview")),f(j,G)};B(it,j=>{a(d)==="/overview"?j(rt):a(d)==="/sessions"?j(st,1):a(je)?j(xt,2):a(d)==="/channels"?j(Ye,3):a(d)==="/hooks"?j(vt,4):a(d)==="/mcp"?j(Ue,5):a(d)==="/skills"?j(wt,6):a(d)==="/plugins"?j(fe,7):a(d)==="/config"?j(Ne,8):a(d)==="/logs"?j(he,9):j(Te,-1)})}N((j,G,qe,at,ut)=>{et(Ke,1,`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${a(s)?"translate-x-0":"-translate-x-full"}`),p(Q,j),p(me,G),p(oe,qe),pt(tt,"aria-label",at),p(ct,aa.lang==="zh"?"中文 / EN":"EN / 中文"),p(H,ut)},[()=>h("app.title"),()=>h("app.menu"),()=>h("app.title"),()=>h("app.language"),()=>h("common.logout")]),J("click",V,()=>b(s,!a(s))),J("click",le,I),J("click",tt,function(...j){da==null||da.apply(this,j)}),J("click",R,O),f(C,D)};B(E,C=>{a(l)?C($,-1):C(g)})}f(e,K),Me()}fr(["click"]);ol(H0,{target:document.getElementById("app")});
