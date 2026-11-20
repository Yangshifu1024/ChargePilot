import { Button, Text, View } from '@tarojs/components'
import { AtIcon } from 'taro-ui'
import { useController } from '../../runtime'
import { TabBar } from '../../runtime/TabBar'
import { Notice } from '../../runtime/ui'
import controller from './index.controller'
import './index.css'

/** 首页只组织公开浏览与进行中订单，支付登录留在充电选择流程。 */
export default function PageView() {
  const { data, event } = useController(controller)
  const stations = data.stations || []
  const announcements = data.announcements || []
  return <>
    <View className='home'>
      <View className='home-brand'>
        <View className='brand-mark'><AtIcon value='lightning-bolt' size={22} color='#d6f26a' /></View>
        <View><Text className='brand-name'>ChargePilot</Text><Text className='page-subtitle'>找桩、扫码、看余额</Text></View>
      </View>
      <View className='hero scan-hero'>
        <View className='hero-body'>
          <Text className='eyebrow'>开始充电</Text>
          <Text className='hero-title'>扫码充电</Text>
          <Text className='hero-desc'>扫描设备或插座二维码</Text>
          <Button className='scan-main' onClick={event('onScanTap')}><AtIcon value='camera' size={22} color='#07281f' /><Text>立即扫码</Text></Button>
          <Button className='scan-manual' onClick={event('onScanTap')}>手动输入编号</Button>
        </View>
      </View>
      {data.ongoing && <View className='ongoing-banner' onClick={event('goOngoing')}>
        <View className='live-dot' />
        <View className='ongoing-body'>
          <Text className='ongoing-label'>{data.ongoing.statusLabel}</Text>
          <Text className='ongoing-meta'>设备 {data.ongoing.device_id} · 插座 {data.ongoing.port_no ?? '—'}</Text>
        </View>
        <Text className='ongoing-action'>查看状态</Text>
      </View>}
      {announcements.length > 0 && <View className='announcement-bar' onClick={event('goAnnouncements')}>
        <AtIcon value='notification' size={18} color='#0e6b52' />
        <Text className='announcement-title'>公告 · {announcements[0].title}</Text>
        <Text className='text-secondary'>›</Text>
      </View>}
      <View className='section-head'><Text className='section-heading'>附近电站</Text><Button className='section-link' onClick={event('goMap')}>查看全部</Button></View>
      {data.locating && <Notice>正在获取位置…</Notice>}
      {data.locationNotice && <Notice retry={event('relocate')}>{data.locationNotice}</Notice>}
      {data.mapError && <Notice tone='error' retry={event('relocate')}>{data.mapError}</Notice>}
      {stations.map((station: any) => <View className='station-card card' key={station.id} onClick={event('goStation', { id: station.id })}>
        <View className='icon-well'><AtIcon value='map-pin' size={18} color='#0e6b52' /></View>
        <View className='station-body'><Text className='station-name'>{station.name || '未命名电站'}</Text><Text className='station-address'>{station.address || '地址未填写'}</Text></View>
        <View className='distance-pill'><Text className='distance-value'>{station.distanceText}</Text><Text className='distance-hint'>查看设备</Text></View>
      </View>)}
      {!stations.length && !data.mapError && !data.locating && <Notice retry={event('relocate')}>暂无可显示的电站</Notice>}
      <View className='page-footnote'>选择站点后查看设备，再选择插座与套餐</View>
    </View>
    <TabBar active='home' />
  </>
}
