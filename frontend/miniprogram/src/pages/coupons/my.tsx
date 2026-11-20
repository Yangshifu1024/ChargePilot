import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { CouponTicket, Empty, Notice, PageHeader, Segments } from '../../runtime/ui'
import controller from './my.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, total, statusIndex, statusNames, loading, error, needsLogin } = data
  return <View className='container'>
    <PageHeader title='我的优惠券' subtitle='充电下单暂不支持使用优惠券' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后查看优惠券</Text>
      <Button type='primary' onClick={event('login', {})}>微信登录</Button>
    </View></View>}
    {!needsLogin && <Segments labels={statusNames} selected={statusIndex} onSelect={index => event('onStatus')({ detail: { value: index } })} />}
    {(items || []).map((item: any, index: number) => <CouponTicket key={String(item?.grant_id ?? index)} name={item?.name} discount={item?.discountText} typeText={item?.typeText} minimumText={item?.minimumText} expiresText={item?.expiresText} statusText={item?.statusText} muted={item?.status !== 'unused'} />)}
    {loading && <Notice>正在读取优惠券…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {!loading && !error && !items?.length && !needsLogin && <Empty icon='tags'>当前没有{statusNames?.[statusIndex]}的优惠券</Empty>}
    {items?.length < total && !loading && <Text className='text-secondary load-more'>继续上滑加载更多</Text>}
    {!needsLogin && <Button onClick={event('goAvailable', {})}>查询可用券</Button>}
    {!needsLogin && <Button type='primary' onClick={event('goScan', {})}>去扫码充电</Button>}
  </View>
}
