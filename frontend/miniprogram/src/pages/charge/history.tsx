import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader, Segments, statusTone } from '../../runtime/ui'
import controller from './history.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, total, loading, error, needsLogin, statusIndex, statusNames } = data
  return <View className='container'>
    <PageHeader title='我的订单' subtitle='按状态查看充电记录' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录查看订单</Text>
      <Button type='primary' onClick={event('login', {})}>登录查看订单</Button>
    </View></View>}
    {!needsLogin && <Segments labels={statusNames} selected={statusIndex} onSelect={index => event('onStatus')({ detail: { value: index } })} />}
    {(items || []).map((item: any, index: number) => <View className='card entity' data-id={item?.order_no} onClick={event('openDetail', { id: item?.order_no })} key={String(item?.order_id ?? index)}>
      <View className='row'><Text className='text-secondary'>{item?.order_no}</Text><Text className={`tag ${statusTone(item?.status)}`}>{item?.statusLabel}</Text></View>
      <View className='entity-title'>{item?.station_name || item?.device_id} · 端口 {item?.port_no}</View>
      <View className='entity-amount'>{item?.totalText}</View>
      <Text className='text-secondary'>{item?.startedText}</Text>
    </View>)}
    {!!error && <Notice tone='error' retry={event('retry', {})} busy={loading}>{error}</Notice>}
    {!error && !loading && !needsLogin && !items?.length && <Empty icon='file-generic'>暂无符合条件的订单</Empty>}
    {!!loading && <Notice>正在加载…</Notice>}
    {!loading && items?.length < total && <Text className='text-secondary load-more'>继续上滑加载更多</Text>}
    {!loading && !(items?.length < total) && !!items?.length && <Text className='text-secondary load-more'>共 {total} 笔，已全部加载</Text>}
  </View>
}
