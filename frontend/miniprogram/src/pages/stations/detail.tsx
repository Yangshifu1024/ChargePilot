import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './detail.controller'
import './detail.css'

/** 站点详情先列设备，不在站点层混排插座与套餐。 */
export default function PageView() {
  const { data, event } = useController(controller)
  return <View className='container station-detail'>
    <PageHeader title='电站详情' subtitle='先选择在线设备，再选择插座与套餐' />
    {data.loading && <Notice>正在读取电站…</Notice>}
    {data.error && <Notice tone='error' retry={event('load')} busy={data.loading}>{data.error}</Notice>}
    {data.station && <>
      <View className='hero'><View className='hero-body'>
        <Text className='eyebrow'>电站</Text>
        <Text className='hero-title'>{data.station.name || '未命名电站'}</Text>
        <Text className='hero-desc'>{data.station.address || '地址未填写'}</Text>
      </View></View>
      {(data.announcements || []).map((notice: any, i: number) => <View className='notice-card' key={notice.id ?? i}>
        <View className='notice-title'><View className='notice-dot' /><Text>{notice.title || '站内公告'}</Text></View>
        <Text className='notice-content'>{notice.content}</Text>
      </View>)}
      <Text className='section-heading'>设备列表</Text>
      {(data.devices || []).map((device: any) => <View className={`card device-card${device.online ? '' : ' device-card-offline'}`} key={device.device_id}>
        <View className='device-main'>
          <Text className='device-id'>设备 {device.device_id}</Text>
          <View className='device-status'><View className={`device-dot${device.online ? ' device-dot-online' : ''}`} /><Text className='device-status-text'>{device.statusText}</Text></View>
        </View>
        <Text className='device-heartbeat'>最近上报：{device.lastHeartbeatText}</Text>
        <Button type={device.clickable ? 'primary' : 'default'} disabled={!device.clickable} onClick={event('openDevice', { id: device.device_id })}>{device.clickable ? '选择此设备' : device.statusText === '离线' ? '设备离线' : '暂不可选择'}</Button>
      </View>)}
      {!data.devices?.length && !data.loading && !data.error && <Empty icon='settings'>该站点暂无设备记录</Empty>}
    </>}
  </View>
}
