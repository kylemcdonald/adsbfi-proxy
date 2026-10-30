const form=document.querySelector('#search'),results=document.querySelector('#results'),message=document.querySelector('#message'),more=document.querySelector('#more');
let cursor=null,params=null,busy=false;
const date=s=>s.slice(0,10),today=new Date();
form.elements.end.value=date(today.toISOString());form.elements.start.value=date(new Date(today-7*86400000).toISOString());
const fmt=s=>s.replace('T',' ').replace('Z','').slice(0,16);
async function load(append=false){
 if(busy)return;busy=true;form.querySelector('button').disabled=true;more.disabled=true;message.textContent='Searching…';
 if(!append){params=new URLSearchParams(new FormData(form));if(params.get('end'))params.set('end',new Date(new Date(params.get('end')+'T00:00:00Z').getTime()+86400000).toISOString());cursor=null;results.replaceChildren();more.hidden=true;}
 if(cursor)params.set('cursor',cursor);else params.delete('cursor');
 try{const response=await fetch('api/flights?'+params),data=await response.json();if(!response.ok)throw Error(data.error||'Search failed');
 for(const row of data.flights){const tr=document.createElement('tr');
 const values=[row.registration||(row.n_number?'N'+row.n_number:row.icao.toUpperCase()),row.owner_name||'—',fmt(row.takeoff),row.takeoff_airport||row.takeoff_airport_id,fmt(row.landing),row.landing_airport||row.landing_airport_id,Math.round((row.landing_ts-row.takeoff_ts)/60)+' min'];
 values.forEach((v,i)=>{const td=document.createElement('td');td.textContent=v;if(i===0){const button=document.createElement('button');button.className='flight-link';button.textContent=v;button.setAttribute('aria-label','View flight path for '+v);td.replaceChildren(button);const small=document.createElement('small');small.textContent=row.icao.toUpperCase()+(row.model?' · '+row.model:'');td.append(small);}if(i===3)td.title=row.takeoff_name||'';if(i===5)td.title=row.landing_name||'';tr.append(td);});tr.className='flight-row';tr.addEventListener('click',()=>{if(!window.getSelection().toString())openFlight(row);});results.append(tr);}
 cursor=data.next_cursor;more.hidden=!cursor;message.textContent=results.children.length?`${results.children.length} flights shown · newest departures first`:'No matching flights';
 }catch(e){message.textContent=e.message;}finally{busy=false;form.querySelector('button').disabled=false;more.disabled=false;}
}
form.addEventListener('submit',e=>{e.preventDefault();load();});more.addEventListener('click',()=>load(true));
let timer;form.elements.airport.addEventListener('input',()=>{clearTimeout(timer);timer=setTimeout(async()=>{try{const q=form.elements.airport.value;const data=await(await fetch('api/airports?q='+encodeURIComponent(q))).json();document.querySelector('#airports').replaceChildren(...data.airports.map(a=>{const o=document.createElement('option');o.value=a.code;o.label=a.name;return o;}));}catch{}},200);});
async function status(){try{const data=await(await fetch('api/status')).json();const w=data.worker;document.querySelector('#status').textContent=`${Number(data.database.event_count||0).toLocaleString()} flights · processed through ${w.processed_through?fmt(w.processed_through):'pending'} UTC`;}catch{document.querySelector('#status').textContent='Database status unavailable';}}
status();setInterval(status,60000);load();

// Map assets load only when a flight is opened.
const flightDialog=document.querySelector('#flight-dialog'),pathStatus=document.querySelector('#path-status');
let mapAssets,mapReady,flightMap,markers=[],pathRequest,selection=0,displayedBounds=null;
function loadMap(){
 if(!mapAssets)mapAssets=new Promise((resolve,reject)=>{
  const css=document.createElement('link');css.rel='stylesheet';css.href='https://unpkg.com/maplibre-gl@5.24.0/dist/maplibre-gl.css';document.head.append(css);
  const script=document.createElement('script');script.src='https://unpkg.com/maplibre-gl@5.24.0/dist/maplibre-gl.js';
  script.onload=resolve;script.onerror=()=>{script.remove();css.remove();mapAssets=null;reject(Error('Map library could not load. Close and reopen to retry.'));};document.head.append(script);
 });
 return mapAssets;
}
async function getMap(){
 await loadMap();
 if(!flightMap){
  flightMap=new maplibregl.Map({container:'flight-map',style:'https://tiles.openfreemap.org/styles/liberty',center:[0,25],zoom:1});
  flightMap.addControl(new maplibregl.NavigationControl());
  flightMap.on('resize',()=>{if(displayedBounds)flightMap.fitBounds(displayedBounds,{padding:45,maxZoom:11,duration:0});});
  mapReady=new Promise((resolve,reject)=>{
   const timeout=setTimeout(()=>{mapReady=null;flightMap.remove();flightMap=null;reject(Error('The basemap could not load. Close and reopen to retry.'));},30000);
   flightMap.once('load',()=>{
    clearTimeout(timeout);
    flightMap.addSource('flight',{type:'geojson',data:{type:'FeatureCollection',features:[]}});
    flightMap.addLayer({id:'route-outline',type:'line',source:'flight',paint:{'line-color':'#ffffff','line-width':7}});
    flightMap.addLayer({id:'route',type:'line',source:'flight',paint:{'line-color':'#087eaa','line-width':3}});
    flightMap.addLayer({id:'isolated-observations',type:'circle',source:'flight',filter:['==',['geometry-type'],'Point'],paint:{'circle-radius':4,'circle-color':'#087eaa','circle-stroke-color':'white','circle-stroke-width':2}});
    resolve(flightMap);
   });
  });
 }
 return mapReady;
}
function clearPath(){
 displayedBounds=null;
 markers.forEach(marker=>marker.remove());markers=[];
 flightMap?.getSource('flight')?.setData({type:'FeatureCollection',features:[]});
}
document.querySelector('#close-flight').addEventListener('click',()=>flightDialog.close());
flightDialog.addEventListener('close',()=>{selection++;pathRequest?.abort();clearPath();});
async function openFlight(row){
 const current=++selection;pathRequest?.abort();pathRequest=new AbortController();clearPath();
 const aircraft=row.registration||(row.n_number?'N'+row.n_number:row.icao.toUpperCase());
 document.querySelector('#flight-title').textContent=aircraft+' · '+(row.takeoff_airport||row.takeoff_airport_id)+' → '+(row.landing_airport||row.landing_airport_id);
 document.querySelector('#flight-details').textContent=fmt(row.takeoff)+' — '+fmt(row.landing)+' UTC'+(row.owner_name?' · '+row.owner_name:'');
 pathStatus.textContent='Loading recorded positions from the local archive…';
 if(!flightDialog.open)flightDialog.showModal();
 flightMap?.resize();
 try{
  const [data,map]=await Promise.all([
   fetch('api/flights/'+row.event_id+'/path',{signal:pathRequest.signal}).then(async response=>{const data=await response.json();if(!response.ok)throw Error(data.error||'Path could not load');return data;}),
   getMap()
  ]);
  if(current!==selection)return;
  map.resize();map.getSource('flight').setData(data.geojson);
  const bounds=data.bounds?data.bounds.map(p=>p.slice()):null,airportCoordinates=[];
  const center=bounds?(bounds[0][0]+bounds[1][0])/2:null;
  for(const [side,label] of [['takeoff','D'],['landing','A']]){
   const f=data.flight,lat=f[side+'_lat'];let lon=f[side+'_lon'];
   if(lat==null||lon==null)continue;
   if(center!=null)lon+=360*Math.round((center-lon)/360);
   airportCoordinates.push([lon,lat]);
   const element=document.createElement('button');element.className='airport-marker'+(side==='landing'?' arrival':'');element.textContent=label;
   const title=(side==='takeoff'?'Departure: ':'Arrival: ')+(f[side+'_airport']||f[side+'_airport_id'])+' · '+(f[side+'_name']||'');
   element.setAttribute('aria-label',title);element.title=title;
   markers.push(new maplibregl.Marker({element}).setLngLat([lon,lat]).setPopup(new maplibregl.Popup({offset:18}).setText(title)).addTo(map));
  }
  const extent=[...(bounds||[]),...airportCoordinates];
  if(extent.length){const fit=new maplibregl.LngLatBounds();extent.forEach(p=>fit.extend(p));displayedBounds=fit;map.fitBounds(fit,{padding:45,maxZoom:11,duration:0});}
  const details=[];
  if(data.missing_slots.length)details.push(data.missing_slots.length+' missing archive files');
  if(data.unreadable_slots.length)details.push(data.unreadable_slots.length+' unreadable archive files');
  if(data.gap_count)details.push(data.gap_count+' observation gaps');
  pathStatus.textContent=data.coverage==='unavailable'
   ?'No recorded path is available in this server’s local cache.'+(details.length?' '+details.join(' · ')+'.':'')
   :data.points.length.toLocaleString()+' recorded positions'+(data.coverage==='partial'?' · incomplete coverage':'')+(details.length?' · '+details.join(' · '):'');
 }catch(error){if(current===selection&&error.name!=='AbortError')pathStatus.textContent=error.message;}
}
