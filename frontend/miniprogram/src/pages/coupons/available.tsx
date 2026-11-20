import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { CouponTicket, Empty, Notice, PageHeader } from '../../runtime/ui'
import controller from './available.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, loading, error, needsLogin } = data
  return <View className='container'>
    <PageHeader title='可用券查询' subtitle='只展示当前可用的优惠券' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录后查询优惠券</Text>
      <Button type='primary' onClick={event('login', {})} loading={loading}>微信登录</Button>
    </View></View>}
    {loading && !items?.length && <Notice>正在读取可用券…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {(items || []).map((item: any, index: number) => <CouponTicket key={String(item?.grant_id ?? index)} name={item?.name} discount={item?.discountText} typeText={item?.typeText} minimumText={item?.minimumText} expiresText={item?.expiresText} statusText='可用' />)}
    {!loading && !error && !items?.length && !needsLogin && <Empty icon='tags'>当前没有符合查询条件的优惠券</Empty>}
    <View className='card'>
      <Text className='text-secondary'>当前页面仅展示优惠券，充电下单暂不支持使用优惠券。</Text>
      <Button onClick={event('goMine', {})}>查看我的全部优惠券</Button>
      <Button type='primary' onClick={event('goScan', {})}>去扫码充电</Button>
    </View>
  </View>
}
