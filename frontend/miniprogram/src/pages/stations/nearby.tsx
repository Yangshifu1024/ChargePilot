import { useState } from 'react'
import { Button, Input, Picker, Text, View } from '@tarojs/components'
import { useController, isH5 } from '../../runtime'
import { inlineStyle, StationMap } from '../../runtime/components'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './nearby.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const [coordinatesOpen, setCoordinatesOpen] = useState(false)
  const { items, markers, latitude, longitude, loading, error, radiusIndex, radii } = data
  return <View className='container'>
    <PageHeader title='附近电站' subtitle='按当前位置与搜索半径列出可去的站点' />
    <View className='toolbar'>
      <Button onClick={event('locate', {})} disabled={loading}>获取当前位置</Button>
      <Picker range={radii} value={radiusIndex} onChange={event('onRadius', {})}><View className='chip'>半径 {radii?.[radiusIndex]} 公里</View></Picker>
    </View>
    {data.locationNotice && <Notice>{data.locationNotice}</Notice>}
    {isH5 && data.locationNotice && <Button onClick={() => setCoordinatesOpen(!coordinatesOpen)}>{coordinatesOpen ? '收起坐标输入' : '定位不可用？手动输入坐标'}</Button>}
    {isH5 && data.locationNotice && coordinatesOpen && <View className='card'>
      <Text className='heading'>按站点坐标搜索</Text>
      <Text className='field-label'>经度</Text>
      <Input placeholder='例如 116.397' value={data.longitudeInput || ''} onInput={event('inputLongitude', {})} />
      <Text className='field-label'>纬度</Text>
      <Input placeholder='例如 39.908' value={data.latitudeInput || ''} onInput={event('inputLatitude', {})} />
      <Button type='primary' onClick={event('useCoordinates', {})} disabled={loading}>按坐标查询</Button>
    </View>}
    {latitude != null && <View className='map-frame'><StationMap latitude={latitude} longitude={longitude} markers={markers} showLocation onMarkerTap={event('open', {})} style={inlineStyle({ width: '100%', height: '420rpx' })} /></View>}
    {!!error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {!!loading && <Notice>正在查询…</Notice>}
    {(items || []).map((item: any, index: number) => <View className='station-card card' data-id={item?.id} onClick={event('open', { id: item?.id })} key={String(item?.id ?? index)}>
      <View className='station-body'><Text className='station-name'>{item?.name}</Text><Text className='station-address'>{item?.address || '地址未填写'}</Text></View>
      <View className='distance-pill'><Text className='distance-value'>{item?.distanceText}</Text><Text className='distance-hint'>查看设备</Text></View>
    </View>)}
    {!loading && !error && !items?.length && <Empty icon='map-pin'>暂无可显示的电站，可重新定位或扩大搜索范围。</Empty>}
  </View>
}
