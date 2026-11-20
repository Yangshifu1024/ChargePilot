import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './list.controller'
import './list.css'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, total, loading, error, needsLogin } = data
  return <View className='container'>
    <PageHeader title='我的账单' subtitle='结算出账后在这里查看' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后查看结算账单</Text>
      <Button type='primary' onClick={event('login', {})} loading={loading}>微信登录</Button>
    </View></View>}
    {(items || []).map((item: any, index: number) => <View className={`card entity bill-card${item?.unread ? ' bill-card-unread' : ''}`} data-id={item?.id} onClick={event('open', { id: item?.id })} key={String(item?.id ?? index)}>
      <View className='row'><Text className='bill-no'>{item?.bill_no}</Text>{item?.unread && <Text className='bill-unread-tag'>未读</Text>}</View>
      <View className='bill-amount'>{item?.totalText}</View>
      <Text className='text-secondary'>{item?.order_no}{item?.device_id ? ` · ${item.device_id}` : ''}</Text>
      <Text className='text-secondary'>出账 {item?.issuedText}</Text>
    </View>)}
    {loading && !items?.length && <Notice>正在读取账单…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {!loading && !error && !items?.length && !needsLogin && <Empty icon='list'>暂无结算账单</Empty>}
    {items?.length > 0 && items?.length < total && !loading && <Text className='text-secondary load-more'>上拉加载更多</Text>}
  </View>
}
